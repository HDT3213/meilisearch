use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::DerefMut as _;

use bumpalo::collections::Vec as BVec;
use bumpalo::Bump;
use hashbrown::HashMap;
use heed::RoTxn;
use serde_json::Value;

use super::super::cache::BalancedCaches;
use super::facet_document::extract_document_facets;
use super::FacetKind;
use crate::heed_codec::facet::OrderedF64Codec;
use crate::update::del_add::DelAdd;
use crate::update::new::channel::FieldIdDocidFacetSender;
use crate::update::new::indexer::document_changes::{
    extract, DocumentChangeContext, DocumentChanges, Extractor, FullySend, IndexingContext,
    Progress, ThreadLocal,
};
use crate::update::new::ref_cell_ext::RefCellExt as _;
use crate::update::new::DocumentChange;
use crate::update::GrenadParameters;
use crate::{DocumentId, FieldId, Index, Result, MAX_FACET_VALUE_LENGTH};

pub struct FacetedExtractorData<'a> {
    attributes_to_extract: &'a [&'a str],
    sender: &'a FieldIdDocidFacetSender<'a>,
    grenad_parameters: GrenadParameters,
    buckets: usize,
}

impl<'a, 'extractor> Extractor<'extractor> for FacetedExtractorData<'a> {
    type Data = RefCell<BalancedCaches<'extractor>>;

    fn init_data(&self, extractor_alloc: &'extractor Bump) -> Result<Self::Data> {
        Ok(RefCell::new(BalancedCaches::new_in(
            self.buckets,
            self.grenad_parameters.max_memory_by_thread(),
            extractor_alloc,
        )))
    }

    fn process<'doc>(
        &self,
        changes: impl Iterator<Item = Result<DocumentChange<'doc>>>,
        context: &DocumentChangeContext<Self::Data>,
    ) -> Result<()> {
        for change in changes {
            let change = change?;
            FacetedDocidsExtractor::extract_document_change(
                context,
                self.attributes_to_extract,
                change,
                self.sender,
            )?
        }
        Ok(())
    }
}

pub struct FacetedDocidsExtractor;

impl FacetedDocidsExtractor {
    fn extract_document_change(
        context: &DocumentChangeContext<RefCell<BalancedCaches>>,
        attributes_to_extract: &[&str],
        document_change: DocumentChange,
        sender: &FieldIdDocidFacetSender,
    ) -> Result<()> {
        let index = &context.index;
        let rtxn = &context.rtxn;
        let mut new_fields_ids_map = context.new_fields_ids_map.borrow_mut_or_yield();
        let mut cached_sorter = context.data.borrow_mut_or_yield();
        let mut del_add_facet_value = DelAddFacetValue::new(&context.doc_alloc);
        let docid = document_change.docid();
        let res = match document_change {
            DocumentChange::Deletion(inner) => extract_document_facets(
                attributes_to_extract,
                inner.current(rtxn, index, context.db_fields_ids_map)?,
                inner.external_document_id(),
                new_fields_ids_map.deref_mut(),
                &mut |fid, value| {
                    Self::facet_fn_with_options(
                        &context.doc_alloc,
                        cached_sorter.deref_mut(),
                        BalancedCaches::insert_del_u32,
                        &mut del_add_facet_value,
                        DelAddFacetValue::insert_del,
                        docid,
                        fid,
                        value,
                    )
                },
            ),
            DocumentChange::Update(inner) => {
                extract_document_facets(
                    attributes_to_extract,
                    inner.current(rtxn, index, context.db_fields_ids_map)?,
                    inner.external_document_id(),
                    new_fields_ids_map.deref_mut(),
                    &mut |fid, value| {
                        Self::facet_fn_with_options(
                            &context.doc_alloc,
                            cached_sorter.deref_mut(),
                            BalancedCaches::insert_del_u32,
                            &mut del_add_facet_value,
                            DelAddFacetValue::insert_del,
                            docid,
                            fid,
                            value,
                        )
                    },
                )?;

                extract_document_facets(
                    attributes_to_extract,
                    inner.merged(rtxn, index, context.db_fields_ids_map)?,
                    inner.external_document_id(),
                    new_fields_ids_map.deref_mut(),
                    &mut |fid, value| {
                        Self::facet_fn_with_options(
                            &context.doc_alloc,
                            cached_sorter.deref_mut(),
                            BalancedCaches::insert_add_u32,
                            &mut del_add_facet_value,
                            DelAddFacetValue::insert_add,
                            docid,
                            fid,
                            value,
                        )
                    },
                )
            }
            DocumentChange::Insertion(inner) => extract_document_facets(
                attributes_to_extract,
                inner.inserted(),
                inner.external_document_id(),
                new_fields_ids_map.deref_mut(),
                &mut |fid, value| {
                    Self::facet_fn_with_options(
                        &context.doc_alloc,
                        cached_sorter.deref_mut(),
                        BalancedCaches::insert_add_u32,
                        &mut del_add_facet_value,
                        DelAddFacetValue::insert_add,
                        docid,
                        fid,
                        value,
                    )
                },
            ),
        };

        del_add_facet_value.send_data(docid, sender, &context.doc_alloc).unwrap();
        res
    }

    fn facet_fn_with_options<'extractor, 'doc>(
        doc_alloc: &'doc Bump,
        cached_sorter: &mut BalancedCaches<'extractor>,
        cache_fn: impl Fn(&mut BalancedCaches<'extractor>, &[u8], u32) -> Result<()>,
        del_add_facet_value: &mut DelAddFacetValue<'doc>,
        facet_fn: impl Fn(&mut DelAddFacetValue<'doc>, FieldId, BVec<'doc, u8>, FacetKind),
        docid: DocumentId,
        fid: FieldId,
        value: &Value,
    ) -> Result<()> {
        let mut buffer = BVec::new_in(doc_alloc);
        // Exists
        // key: fid
        buffer.push(FacetKind::Exists as u8);
        buffer.extend_from_slice(&fid.to_be_bytes());
        cache_fn(cached_sorter, &buffer, docid)?;

        match value {
            // Number
            // key: fid - level - orderedf64 - orignalf64
            Value::Number(number) => {
                let mut ordered = [0u8; 16];
                if number
                    .as_f64()
                    .and_then(|n| OrderedF64Codec::serialize_into(n, &mut ordered).ok())
                    .is_some()
                {
                    let mut number = BVec::with_capacity_in(16, doc_alloc);
                    number.extend_from_slice(&ordered);
                    facet_fn(del_add_facet_value, fid, number, FacetKind::Number);

                    buffer.clear();
                    buffer.push(FacetKind::Number as u8);
                    buffer.extend_from_slice(&fid.to_be_bytes());
                    buffer.push(0); // level 0
                    buffer.extend_from_slice(&ordered);
                    cache_fn(cached_sorter, &buffer, docid)
                } else {
                    Ok(())
                }
            }
            // String
            // key: fid - level - truncated_string
            Value::String(s) => {
                let mut string = BVec::new_in(doc_alloc);
                string.extend_from_slice(s.as_bytes());
                facet_fn(del_add_facet_value, fid, string, FacetKind::String);

                let normalized = crate::normalize_facet(s);
                let truncated = truncate_str(&normalized);
                buffer.clear();
                buffer.push(FacetKind::String as u8);
                buffer.extend_from_slice(&fid.to_be_bytes());
                buffer.push(0); // level 0
                buffer.extend_from_slice(truncated.as_bytes());
                cache_fn(cached_sorter, &buffer, docid)
            }
            // Null
            // key: fid
            Value::Null => {
                buffer.clear();
                buffer.push(FacetKind::Null as u8);
                buffer.extend_from_slice(&fid.to_be_bytes());
                cache_fn(cached_sorter, &buffer, docid)
            }
            // Empty
            // key: fid
            Value::Array(a) if a.is_empty() => {
                buffer.clear();
                buffer.push(FacetKind::Empty as u8);
                buffer.extend_from_slice(&fid.to_be_bytes());
                cache_fn(cached_sorter, &buffer, docid)
            }
            Value::Object(o) if o.is_empty() => {
                buffer.clear();
                buffer.push(FacetKind::Empty as u8);
                buffer.extend_from_slice(&fid.to_be_bytes());
                cache_fn(cached_sorter, &buffer, docid)
            }
            // Otherwise, do nothing
            /// TODO: What about Value::Bool?
            _ => Ok(()),
        }
    }

    fn attributes_to_extract<'a>(rtxn: &'a RoTxn, index: &'a Index) -> Result<HashSet<String>> {
        index.user_defined_faceted_fields(rtxn)
    }
}

struct DelAddFacetValue<'doc> {
    strings: HashMap<(FieldId, BVec<'doc, u8>), DelAdd, hashbrown::DefaultHashBuilder, &'doc Bump>,
    f64s: HashMap<(FieldId, BVec<'doc, u8>), DelAdd, hashbrown::DefaultHashBuilder, &'doc Bump>,
}

impl<'doc> DelAddFacetValue<'doc> {
    fn new(doc_alloc: &'doc Bump) -> Self {
        Self { strings: HashMap::new_in(doc_alloc), f64s: HashMap::new_in(doc_alloc) }
    }

    fn insert_add(&mut self, fid: FieldId, value: BVec<'doc, u8>, kind: FacetKind) {
        let cache = match kind {
            FacetKind::String => &mut self.strings,
            FacetKind::Number => &mut self.f64s,
            _ => return,
        };

        let key = (fid, value);
        if let Some(DelAdd::Deletion) = cache.get(&key) {
            cache.remove(&key);
        } else {
            cache.insert(key, DelAdd::Addition);
        }
    }

    fn insert_del(&mut self, fid: FieldId, value: BVec<'doc, u8>, kind: FacetKind) {
        let cache = match kind {
            FacetKind::String => &mut self.strings,
            FacetKind::Number => &mut self.f64s,
            _ => return,
        };

        let key = (fid, value);
        if let Some(DelAdd::Addition) = cache.get(&key) {
            cache.remove(&key);
        } else {
            cache.insert(key, DelAdd::Deletion);
        }
    }

    fn send_data(
        self,
        docid: DocumentId,
        sender: &FieldIdDocidFacetSender,
        doc_alloc: &Bump,
    ) -> std::result::Result<(), crossbeam_channel::SendError<()>> {
        let mut buffer = bumpalo::collections::Vec::new_in(doc_alloc);
        for ((fid, value), deladd) in self.strings {
            if let Ok(s) = std::str::from_utf8(&value) {
                buffer.clear();
                buffer.extend_from_slice(&fid.to_be_bytes());
                buffer.extend_from_slice(&docid.to_be_bytes());
                let normalized = crate::normalize_facet(s);
                let truncated = truncate_str(&normalized);
                buffer.extend_from_slice(truncated.as_bytes());
                match deladd {
                    DelAdd::Deletion => sender.delete_facet_string(&buffer)?,
                    DelAdd::Addition => sender.write_facet_string(&buffer, &value)?,
                }
            }
        }

        for ((fid, value), deladd) in self.f64s {
            buffer.clear();
            buffer.extend_from_slice(&fid.to_be_bytes());
            buffer.extend_from_slice(&docid.to_be_bytes());
            buffer.extend_from_slice(&value);
            match deladd {
                DelAdd::Deletion => sender.delete_facet_f64(&buffer)?,
                DelAdd::Addition => sender.write_facet_f64(&buffer)?,
            }
        }

        Ok(())
    }
}

/// Truncates a string to the biggest valid LMDB key size.
fn truncate_str(s: &str) -> &str {
    let index = s
        .char_indices()
        .map(|(idx, _)| idx)
        .chain(std::iter::once(s.len()))
        .take_while(|idx| idx <= &MAX_FACET_VALUE_LENGTH)
        .last();

    &s[..index.unwrap_or(0)]
}

impl FacetedDocidsExtractor {
    #[tracing::instrument(level = "trace", skip_all, target = "indexing::extract::faceted")]
    pub fn run_extraction<
        'pl,
        'fid,
        'indexer,
        'index,
        'extractor,
        DC: DocumentChanges<'pl>,
        MSP,
        SP,
    >(
        grenad_parameters: GrenadParameters,
        document_changes: &DC,
        indexing_context: IndexingContext<'fid, 'indexer, 'index, MSP, SP>,
        extractor_allocs: &'extractor mut ThreadLocal<FullySend<Bump>>,
        sender: &FieldIdDocidFacetSender,
        finished_steps: u16,
        total_steps: u16,
        step_name: &'static str,
    ) -> Result<Vec<BalancedCaches<'extractor>>>
    where
        MSP: Fn() -> bool + Sync,
        SP: Fn(Progress) + Sync,
    {
        let index = indexing_context.index;
        let rtxn = index.read_txn()?;
        let attributes_to_extract = Self::attributes_to_extract(&rtxn, index)?;
        let attributes_to_extract: Vec<_> =
            attributes_to_extract.iter().map(|s| s.as_ref()).collect();
        let datastore = ThreadLocal::new();

        {
            let span =
                tracing::trace_span!(target: "indexing::documents::extract", "docids_extraction");
            let _entered = span.enter();

            let extractor = FacetedExtractorData {
                attributes_to_extract: &attributes_to_extract,
                grenad_parameters,
                buckets: rayon::current_num_threads(),
                sender,
            };
            extract(
                document_changes,
                &extractor,
                indexing_context,
                extractor_allocs,
                &datastore,
                finished_steps,
                total_steps,
                step_name,
            )?;
        }

        Ok(datastore.into_iter().map(RefCell::into_inner).collect())
    }
}
