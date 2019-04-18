use std::collections::HashSet;
use std::io::{self, Cursor, BufRead};
use std::iter::FromIterator;
use std::path::Path;
use std::sync::Arc;

use arc_swap::{ArcSwap, Lease};
use hashbrown::HashMap;
use meilidb_core::shared_data_cursor::{FromSharedDataCursor, SharedDataCursor};
use meilidb_core::write_to_bytes::WriteToBytes;
use meilidb_core::{DocumentId, Index as WordIndex};
use rmp_serde::decode::{Deserializer as RmpDeserializer, ReadReader};
use rmp_serde::decode::{Error as RmpError};
use serde::{de, forward_to_deserialize_any};
use sled::IVec;
use byteorder::{ReadBytesExt, BigEndian};

use crate::{Schema, SchemaAttr, RankedMap};

#[derive(Debug)]
pub enum Error {
    SchemaDiffer,
    SchemaMissing,
    WordIndexMissing,
    SledError(sled::Error),
    BincodeError(bincode::Error),
}

impl From<sled::Error> for Error {
    fn from(error: sled::Error) -> Error {
        Error::SledError(error)
    }
}

impl From<bincode::Error> for Error {
    fn from(error: bincode::Error) -> Error {
        Error::BincodeError(error)
    }
}

fn index_name(name: &str) -> Vec<u8> {
    format!("index-{}", name).into_bytes()
}

fn document_key(id: DocumentId, attr: SchemaAttr) -> Vec<u8> {
    let DocumentId(document_id) = id;
    let SchemaAttr(schema_attr) = attr;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"document-");
    bytes.extend_from_slice(&document_id.to_be_bytes()[..]);
    bytes.extend_from_slice(&schema_attr.to_be_bytes()[..]);
    bytes
}

trait CursorExt {
    fn consume_if_eq(&mut self, needle: &[u8]) -> bool;
}

impl<T: AsRef<[u8]>> CursorExt for Cursor<T> {
    fn consume_if_eq(&mut self, needle: &[u8]) -> bool {
        let position = self.position() as usize;
        let slice = self.get_ref().as_ref();

        if slice[position..].starts_with(needle) {
            self.consume(needle.len());
            true
        } else {
            false
        }
    }
}

fn extract_document_key(key: Vec<u8>) -> io::Result<(DocumentId, SchemaAttr)> {
    let mut key = Cursor::new(key);

    if !key.consume_if_eq(b"document-") {
        return Err(io::Error::from(io::ErrorKind::InvalidData))
    }

    let document_id = key.read_u64::<BigEndian>().map(DocumentId)?;
    let schema_attr = key.read_u16::<BigEndian>().map(SchemaAttr)?;

    Ok((document_id, schema_attr))
}

fn ivec_into_arc(ivec: IVec) -> Arc<[u8]> {
    match ivec {
        IVec::Inline(len, bytes) => Arc::from(&bytes[..len as usize]),
        IVec::Remote { buf } => buf,
    }
}

#[derive(Clone)]
pub struct Database {
    opened: Arc<ArcSwap<HashMap<String, RawIndex>>>,
    inner: sled::Db,
}

impl Database {
    pub fn start_default<P: AsRef<Path>>(path: P) -> Result<Database, Error> {
        let inner = sled::Db::start_default(path)?;
        let opened = Arc::new(ArcSwap::new(Arc::new(HashMap::new())));
        Ok(Database { opened, inner })
    }

    pub fn open_index(&self, name: &str) -> Result<Option<Index>, Error> {
        // check if the index was already opened
        if let Some(raw_index) = self.opened.lease().get(name) {
            return Ok(Some(Index(raw_index.clone())))
        }

        let raw_name = index_name(name);
        if self.inner.tree_names().into_iter().any(|tn| tn == raw_name) {
            let tree = self.inner.open_tree(raw_name)?;
            let raw_index = RawIndex::from_raw(tree)?;

            self.opened.rcu(|opened| {
                let mut opened = HashMap::clone(opened);
                opened.insert(name.to_string(), raw_index.clone());
                opened
            });

            return Ok(Some(Index(raw_index)))
        }

        Ok(None)
    }

    pub fn create_index(&self, name: String, schema: Schema) -> Result<Index, Error> {
        match self.open_index(&name)? {
            Some(index) => {
                if index.schema() != &schema {
                    return Err(Error::SchemaDiffer);
                }

                Ok(index)
            },
            None => {
                let raw_name = index_name(&name);
                let tree = self.inner.open_tree(raw_name)?;
                let raw_index = RawIndex::new_from_raw(tree, schema)?;

                self.opened.rcu(|opened| {
                    let mut opened = HashMap::clone(opened);
                    opened.insert(name.clone(), raw_index.clone());
                    opened
                });

                Ok(Index(raw_index))
            },
        }
    }
}

#[derive(Clone)]
pub struct RawIndex {
    schema: Schema,
    word_index: Arc<ArcSwap<WordIndex>>,
    ranked_map: Arc<ArcSwap<RankedMap>>,
    inner: Arc<sled::Tree>,
}

impl RawIndex {
    fn from_raw(inner: Arc<sled::Tree>) -> Result<RawIndex, Error> {
        let schema = {
            let bytes = inner.get("schema")?;
            let bytes = bytes.ok_or(Error::SchemaMissing)?;
            Schema::read_from_bin(bytes.as_ref())?
        };

        let bytes = inner.get("word-index")?;
        let bytes = bytes.ok_or(Error::WordIndexMissing)?;
        let word_index = {
            let len = bytes.len();
            let bytes = ivec_into_arc(bytes);
            let mut cursor = SharedDataCursor::from_shared_bytes(bytes, 0, len);

            // TODO must handle this error
            let word_index = WordIndex::from_shared_data_cursor(&mut cursor).unwrap();

            Arc::new(ArcSwap::new(Arc::new(word_index)))
        };

        let ranked_map = {
            let map = match inner.get("ranked-map")? {
                Some(bytes) => bincode::deserialize(bytes.as_ref())?,
                None => RankedMap::default(),
            };

            Arc::new(ArcSwap::new(Arc::new(map)))
        };

        Ok(RawIndex { schema, word_index, ranked_map, inner })
    }

    fn new_from_raw(inner: Arc<sled::Tree>, schema: Schema) -> Result<RawIndex, Error> {
        let mut schema_bytes = Vec::new();
        schema.write_to_bin(&mut schema_bytes)?;
        inner.set("schema", schema_bytes)?;

        let word_index = WordIndex::default();
        inner.set("word-index", word_index.into_bytes())?;
        let word_index = Arc::new(ArcSwap::new(Arc::new(word_index)));

        let ranked_map = Arc::new(ArcSwap::new(Arc::new(RankedMap::default())));

        Ok(RawIndex { schema, word_index, ranked_map, inner })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn word_index(&self) -> Lease<Arc<WordIndex>> {
        self.word_index.lease()
    }

    pub fn ranked_map(&self) -> Lease<Arc<RankedMap>> {
        self.ranked_map.lease()
    }

    pub fn update_word_index(&self, word_index: Arc<WordIndex>) {
        self.word_index.store(word_index)
    }

    pub fn update_ranked_map(&self, ranked_map: Arc<RankedMap>) {
        self.ranked_map.store(ranked_map)
    }

    pub fn set_document_attribute<V>(
        &self,
        id: DocumentId,
        attr: SchemaAttr,
        value: V,
    ) -> Result<Option<IVec>, Error>
    where IVec: From<V>,
    {
        let key = document_key(id, attr);
        Ok(self.inner.set(key, value)?)
    }

    pub fn get_document_attribute(
        &self,
        id: DocumentId,
        attr: SchemaAttr
    ) -> Result<Option<IVec>, Error>
    {
        let key = document_key(id, attr);
        Ok(self.inner.get(key)?)
    }

    pub fn get_document_fields(&self, id: DocumentId) -> DocumentFieldsIter {
        let start = document_key(id, SchemaAttr::min());
        let end = document_key(id, SchemaAttr::max());
        DocumentFieldsIter(self.inner.range(start..=end))
    }

    pub fn del_document_attribute(
        &self,
        id: DocumentId,
        attr: SchemaAttr
    ) -> Result<Option<IVec>, Error>
    {
        let key = document_key(id, attr);
        Ok(self.inner.del(key)?)
    }
}

pub struct DocumentFieldsIter<'a>(sled::Iter<'a>);

impl<'a> Iterator for DocumentFieldsIter<'a> {
    type Item = Result<(DocumentId, SchemaAttr, IVec), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Some(Ok((key, value))) => {
                let (id, attr) = extract_document_key(key).unwrap();
                Some(Ok((id, attr, value)))
            },
            Some(Err(e)) => Some(Err(Error::SledError(e))),
            None => None,
        }
    }
}

#[derive(Clone)]
pub struct Index(RawIndex);

impl Index {
    pub fn schema(&self) -> &Schema {
        self.0.schema()
    }

    pub fn word_index(&self) -> Lease<Arc<WordIndex>> {
        self.0.word_index()
    }

    pub fn ranked_map(&self) -> Lease<Arc<RankedMap>> {
        self.0.ranked_map()
    }

    pub fn document<T>(
        &self,
        fields: Option<&HashSet<&str>>,
        id: DocumentId,
    ) -> Result<Option<T>, RmpError>
    where T: de::DeserializeOwned,
    {
        let fields = match fields {
            Some(fields) => {
                let iter = fields.iter().filter_map(|n| self.0.schema().attribute(n));
                Some(HashSet::from_iter(iter))
            },
            None => None,
        };

        let mut deserializer = Deserializer {
            document_id: id,
            raw_index: &self.0,
            fields: fields.as_ref(),
        };

        // TODO: currently we return an error if all document fields are missing,
        //       returning None would have been better
        T::deserialize(&mut deserializer).map(Some)
    }
}

struct Deserializer<'a> {
    document_id: DocumentId,
    raw_index: &'a RawIndex,
    fields: Option<&'a HashSet<SchemaAttr>>,
}

impl<'de, 'a, 'b> de::Deserializer<'de> for &'b mut Deserializer<'a>
{
    type Error = RmpError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where V: de::Visitor<'de>
    {
        self.deserialize_map(visitor)
    }

    forward_to_deserialize_any! {
        bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string unit seq
        bytes byte_buf unit_struct tuple_struct
        identifier tuple ignored_any option newtype_struct enum struct
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where V: de::Visitor<'de>
    {
        let document_attributes = self.raw_index.get_document_fields(self.document_id);
        let document_attributes = document_attributes.filter_map(|result| {
            match result {
                Ok(value) => Some(value),
                Err(e) => {
                    // TODO: must log the error
                    // error!("sled iter error; {}", e);
                    None
                },
            }
        });
        let iter = document_attributes.filter_map(|(_, attr, value)| {
            if self.fields.map_or(true, |f| f.contains(&attr)) {
                let attribute_name = self.raw_index.schema.attribute_name(attr);
                Some((attribute_name, Value::new(value)))
            } else {
                None
            }
        });

        let map_deserializer = de::value::MapDeserializer::new(iter);
        visitor.visit_map(map_deserializer)
    }
}

struct Value<A>(RmpDeserializer<ReadReader<Cursor<A>>>) where A: AsRef<[u8]>;

impl<A> Value<A> where A: AsRef<[u8]>
{
    fn new(value: A) -> Value<A> {
        Value(RmpDeserializer::new(Cursor::new(value)))
    }
}

impl<'de, A> de::IntoDeserializer<'de, RmpError> for Value<A>
where A: AsRef<[u8]>,
{
    type Deserializer = Self;

    fn into_deserializer(self) -> Self::Deserializer {
        self
    }
}

impl<'de, 'a, A> de::Deserializer<'de> for Value<A>
where A: AsRef<[u8]>,
{
    type Error = RmpError;

    fn deserialize_any<V>(mut self, visitor: V) -> Result<V::Value, Self::Error>
    where V: de::Visitor<'de>
    {
        self.0.deserialize_any(visitor)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}
