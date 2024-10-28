use std::collections::BTreeSet;

use bumpalo::Bump;
use heed::RoTxn;
use raw_collections::RawMap;
use serde::Serialize;
use serde_json::value::RawValue;

use super::document::{Document, DocumentFromDb, DocumentFromVersions, Versions};
use crate::documents::FieldIdMapper;
use crate::index::IndexEmbeddingConfig;
use crate::vector::parsed_vectors::RawVectors;
use crate::vector::Embedding;
use crate::{DocumentId, Index, InternalError, Result, UserError};

#[derive(Serialize)]
#[serde(untagged)]
pub enum Embeddings<'doc> {
    FromJson(&'doc RawValue),
    FromDb(Vec<Embedding>),
}
impl<'doc> Embeddings<'doc> {
    pub fn into_vec(self) -> std::result::Result<Vec<Embedding>, serde_json::Error> {
        match self {
            /// FIXME: this should be a VecOrArrayOfVec
            Embeddings::FromJson(value) => serde_json::from_str(value.get()),
            Embeddings::FromDb(vec) => Ok(vec),
        }
    }
}

pub struct VectorEntry<'doc> {
    pub has_configured_embedder: bool,
    pub embeddings: Option<Embeddings<'doc>>,
    pub regenerate: bool,
}

pub trait VectorDocument<'doc> {
    fn iter_vectors(&self) -> impl Iterator<Item = Result<(&'doc str, VectorEntry<'doc>)>>;

    fn vectors_for_key(&self, key: &str) -> Result<Option<VectorEntry<'doc>>>;
}

pub struct VectorDocumentFromDb<'t> {
    docid: DocumentId,
    embedding_config: Vec<IndexEmbeddingConfig>,
    index: &'t Index,
    vectors_field: Option<RawMap<'t>>,
    rtxn: &'t RoTxn<'t>,
    doc_alloc: &'t Bump,
}

impl<'t> VectorDocumentFromDb<'t> {
    pub fn new<Mapper: FieldIdMapper>(
        docid: DocumentId,
        index: &'t Index,
        rtxn: &'t RoTxn,
        db_fields_ids_map: &'t Mapper,
        doc_alloc: &'t Bump,
    ) -> Result<Option<Self>> {
        let Some(document) = DocumentFromDb::new(docid, rtxn, index, db_fields_ids_map)? else {
            return Ok(None);
        };
        let vectors = document.vectors_field()?;
        let vectors_field = match vectors {
            Some(vectors) => {
                Some(RawMap::from_raw_value(vectors, doc_alloc).map_err(InternalError::SerdeJson)?)
            }
            None => None,
        };

        let embedding_config = index.embedding_configs(rtxn)?;

        Ok(Some(Self { docid, embedding_config, index, vectors_field, rtxn, doc_alloc }))
    }

    fn entry_from_db(
        &self,
        embedder_id: u8,
        config: &IndexEmbeddingConfig,
    ) -> Result<VectorEntry<'t>> {
        let readers = self.index.arroy_readers(self.rtxn, embedder_id, config.config.quantized());
        let mut vectors = Vec::new();
        for reader in readers {
            let reader = reader?;
            let Some(vector) = reader.item_vector(self.rtxn, self.docid)? else {
                break;
            };

            vectors.push(vector);
        }
        Ok(VectorEntry {
            has_configured_embedder: true,
            embeddings: Some(Embeddings::FromDb(vectors)),
            regenerate: !config.user_provided.contains(self.docid),
        })
    }
}

impl<'t> VectorDocument<'t> for VectorDocumentFromDb<'t> {
    fn iter_vectors(&self) -> impl Iterator<Item = Result<(&'t str, VectorEntry<'t>)>> {
        self.embedding_config
            .iter()
            .map(|config| {
                let embedder_id =
                    self.index.embedder_category_id.get(self.rtxn, &config.name)?.unwrap();
                let entry = self.entry_from_db(embedder_id, config)?;
                let config_name = self.doc_alloc.alloc_str(config.name.as_str());
                Ok((&*config_name, entry))
            })
            .chain(self.vectors_field.iter().flat_map(|map| map.iter()).map(|(name, value)| {
                Ok((name, entry_from_raw_value(value).map_err(InternalError::SerdeJson)?))
            }))
    }

    fn vectors_for_key(&self, key: &str) -> Result<Option<VectorEntry<'t>>> {
        Ok(match self.index.embedder_category_id.get(self.rtxn, key)? {
            Some(embedder_id) => {
                let config =
                    self.embedding_config.iter().find(|config| config.name == key).unwrap();
                Some(self.entry_from_db(embedder_id, config)?)
            }
            None => match self.vectors_field.as_ref().and_then(|obkv| obkv.get(key)) {
                Some(embedding_from_doc) => Some(
                    entry_from_raw_value(embedding_from_doc).map_err(InternalError::SerdeJson)?,
                ),
                None => None,
            },
        })
    }
}

fn entry_from_raw_value(
    value: &RawValue,
) -> std::result::Result<VectorEntry<'_>, serde_json::Error> {
    let value: RawVectors = serde_json::from_str(value.get())?;
    Ok(VectorEntry {
        has_configured_embedder: false,
        embeddings: value.embeddings().map(Embeddings::FromJson),
        regenerate: value.must_regenerate(),
    })
}

pub struct VectorDocumentFromVersions<'doc> {
    vectors: RawMap<'doc>,
}

impl<'doc> VectorDocumentFromVersions<'doc> {
    pub fn new(versions: &Versions<'doc>, bump: &'doc Bump) -> Result<Option<Self>> {
        let document = DocumentFromVersions::new(versions);
        if let Some(vectors_field) = document.vectors_field()? {
            let vectors =
                RawMap::from_raw_value(vectors_field, bump).map_err(UserError::SerdeJson)?;
            Ok(Some(Self { vectors }))
        } else {
            Ok(None)
        }
    }
}

impl<'doc> VectorDocument<'doc> for VectorDocumentFromVersions<'doc> {
    fn iter_vectors(&self) -> impl Iterator<Item = Result<(&'doc str, VectorEntry<'doc>)>> {
        self.vectors.iter().map(|(embedder, vectors)| {
            let vectors = entry_from_raw_value(vectors).map_err(UserError::SerdeJson)?;
            Ok((embedder, vectors))
        })
    }

    fn vectors_for_key(&self, key: &str) -> Result<Option<VectorEntry<'doc>>> {
        let Some(vectors) = self.vectors.get(key) else { return Ok(None) };
        let vectors = entry_from_raw_value(vectors).map_err(UserError::SerdeJson)?;
        Ok(Some(vectors))
    }
}

pub struct MergedVectorDocument<'doc> {
    new_doc: Option<VectorDocumentFromVersions<'doc>>,
    db: Option<VectorDocumentFromDb<'doc>>,
}

impl<'doc> MergedVectorDocument<'doc> {
    pub fn with_db<Mapper: FieldIdMapper>(
        docid: DocumentId,
        index: &'doc Index,
        rtxn: &'doc RoTxn,
        db_fields_ids_map: &'doc Mapper,
        versions: &Versions<'doc>,
        doc_alloc: &'doc Bump,
    ) -> Result<Option<Self>> {
        let db = VectorDocumentFromDb::new(docid, index, rtxn, db_fields_ids_map, doc_alloc)?;
        let new_doc = VectorDocumentFromVersions::new(versions, doc_alloc)?;
        Ok(if db.is_none() && new_doc.is_none() { None } else { Some(Self { new_doc, db }) })
    }

    pub fn without_db(versions: &Versions<'doc>, doc_alloc: &'doc Bump) -> Result<Option<Self>> {
        let Some(new_doc) = VectorDocumentFromVersions::new(versions, doc_alloc)? else {
            return Ok(None);
        };
        Ok(Some(Self { new_doc: Some(new_doc), db: None }))
    }
}

impl<'doc> VectorDocument<'doc> for MergedVectorDocument<'doc> {
    fn iter_vectors(&self) -> impl Iterator<Item = Result<(&'doc str, VectorEntry<'doc>)>> {
        let mut new_doc_it = self.new_doc.iter().flat_map(|new_doc| new_doc.iter_vectors());
        let mut db_it = self.db.iter().flat_map(|db| db.iter_vectors());
        let mut seen_fields = BTreeSet::new();

        std::iter::from_fn(move || {
            if let Some(next) = new_doc_it.next() {
                if let Ok((name, _)) = next {
                    seen_fields.insert(name);
                }
                return Some(next);
            }
            loop {
                match db_it.next()? {
                    Ok((name, value)) => {
                        if seen_fields.contains(name) {
                            continue;
                        }
                        return Some(Ok((name, value)));
                    }
                    Err(err) => return Some(Err(err)),
                }
            }
        })
    }

    fn vectors_for_key(&self, key: &str) -> Result<Option<VectorEntry<'doc>>> {
        if let Some(new_doc) = &self.new_doc {
            if let Some(entry) = new_doc.vectors_for_key(key)? {
                return Ok(Some(entry));
            }
        }

        let Some(db) = self.db.as_ref() else { return Ok(None) };
        db.vectors_for_key(key)
    }
}
