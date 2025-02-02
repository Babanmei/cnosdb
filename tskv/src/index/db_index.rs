use bytes::BufMut;
use chrono::format::format;
use std::borrow::Borrow;
use std::mem::size_of;
use std::ops::Index;
use std::path;
use std::{collections::HashMap, sync::Arc};

use super::*;
use config::GlobalConfig;
use models::{FieldId, FieldInfo, SeriesInfo, SeriesKey, Tag, ValueType};

const SERIES_KEY_PREFIX: &str = "_series_key_";
const TABLE_SCHEMA_PREFIX: &str = "_table_schema_";

#[derive(Clone)]
pub struct IndexConfig {
    pub path: String,
}

impl From<&GlobalConfig> for IndexConfig {
    fn from(config: &GlobalConfig) -> Self {
        Self {
            path: config.index_path.clone(),
        }
    }
}

pub struct DBIndex {
    path: path::PathBuf,

    storage: engine::IndexEngine,
    series_cache: HashMap<u32, Vec<SeriesKey>>,

    table_schema: HashMap<String, Vec<FieldInfo>>,
}

impl From<&str> for DBIndex {
    fn from(path: &str) -> Self {
        DBIndex::new(&path.to_string())
    }
}

impl DBIndex {
    pub fn new(path: &String) -> Self {
        Self {
            storage: engine::IndexEngine::new(path),
            series_cache: HashMap::new(),
            table_schema: HashMap::new(),

            path: path::Path::new(&path).to_path_buf(),
        }
    }

    pub async fn add_series_if_not_exists(
        &mut self,
        info: &mut SeriesInfo,
    ) -> errors::IndexResult<u64> {
        let mut series_key = SeriesKey::from(info.borrow());
        let (hash_id, _) = utils::split_id(series_key.hash());
        let stroage_key = format!("{}{}", SERIES_KEY_PREFIX, hash_id.to_string());

        // load index first from cache,or else from storage and than cache it!
        let mut keys: &mut Vec<SeriesKey> = &mut vec![];
        match self.series_cache.get_mut(&hash_id) {
            Some(v) => keys = v,
            None => {
                if let Some(data) = self.storage.get(stroage_key.as_bytes())? {
                    if let Ok(v) = bincode::deserialize(&data) {
                        self.series_cache.insert(hash_id, v);
                        keys = self.series_cache.get_mut(&hash_id).unwrap();
                    }
                }
            }
        }

        //if exist return series_id
        if let Some(k) = keys.iter().find(|key| series_key.eq(key)) {
            return Ok(k.id());
        }

        //if not exist add it!
        let id = utils::unite_id(hash_id as u64, self.storage.incr_id()?);
        series_key.set_id(id);
        for tag in series_key.tags() {
            let key = utils::encode_inverted_index_key(series_key.table(), &tag.key, &tag.value);
            self.storage.push(&key, &id.to_be_bytes().to_vec())?;
        }

        keys.push(series_key);
        self.storage
            .set(stroage_key.as_bytes(), &bincode::serialize(&keys).unwrap())?;

        self.check_field_type_or_else_add(id, info)?;

        Ok(id)
    }

    fn check_field_type_or_else_add(
        &mut self,
        series_id: u64,
        info: &mut SeriesInfo,
    ) -> IndexResult<()> {
        //load schema first from cache,or else from storage and than cache it!
        let mut schema: &mut Vec<FieldInfo> = &mut vec![];
        match self.table_schema.get_mut(info.table()) {
            Some(fields) => schema = fields,
            None => {
                let key = format!("{}{}", TABLE_SCHEMA_PREFIX, info.table());
                if let Some(data) = self.storage.get(key.as_bytes())? {
                    if let Ok(list) = bincode::deserialize(&data) {
                        self.table_schema.insert(info.table().to_string(), list);
                        schema = self.table_schema.get_mut(info.table()).unwrap();
                    }
                }
            }
        }

        let mut need_store = false;
        let mut check_fn = |field: &mut FieldInfo| -> IndexResult<()> {
            match schema.iter().find(|item| field.eq(item)) {
                Some(v) => {
                    if field.value_type() != v.value_type() {
                        return Err(IndexError::FieldType);
                    }

                    field.set_field_id(v.field_id());
                }
                None => {
                    let field_id = utils::unite_id((schema.len() + 1) as u64, series_id);

                    field.set_field_id(field_id);
                    schema.push(field.clone());

                    need_store = true;
                }
            }
            Ok(())
        };

        //check fields
        for field in info.field_infos() {
            check_fn(field)?
        }

        //check tags
        for tag in info.tags() {
            check_fn(&mut FieldInfo::from(tag))?
        }

        //if needed, store it
        if need_store {
            let data = bincode::serialize(schema).unwrap();
            let key = format!("{}{}", TABLE_SCHEMA_PREFIX, info.table());
            let data = self.storage.set(key.as_bytes(), &data)?;
        }

        Ok(())
    }

    pub async fn get_table_schema(&mut self, tab: &String) -> IndexResult<Option<Vec<FieldInfo>>> {
        if let Some(fields) = self.table_schema.get(tab) {
            return Ok(Some(fields.to_vec()));
        }

        let key = format!("{}{}", TABLE_SCHEMA_PREFIX, tab);
        if let Some(data) = self.storage.get(key.as_bytes())? {
            if let Ok(list) = bincode::deserialize::<Vec<FieldInfo>>(&data) {
                let clone = list.to_vec();
                self.table_schema.insert(tab.to_string(), list);
                return Ok(Some(clone));
            }
        }

        Ok(None)
    }

    pub async fn del_table_schema(&mut self, tab: &String) -> IndexResult<()> {
        self.table_schema.remove(tab);

        let key = format!("{}{}", TABLE_SCHEMA_PREFIX, tab);
        self.storage.delete(key.as_bytes())?;

        Ok(())
    }

    pub async fn close(&mut self) -> IndexResult<()> {
        self.storage.close();
        Ok(())
    }

    pub fn get_series_info_list(&self, sids: &[u64]) -> Vec<SeriesInfo> {
        todo!()
    }

    pub async fn get_series_key(&mut self, sid: u64) -> IndexResult<Option<SeriesKey>> {
        let (hash_id, _) = utils::split_id(sid);
        let stroage_key = format!("{}{}", SERIES_KEY_PREFIX, hash_id.to_string());

        // load index first from cache,or else from storage and than cache it!
        let mut keys: &mut Vec<SeriesKey> = &mut vec![];
        if let Some(v) = self.series_cache.get_mut(&hash_id) {
            keys = v;
        } else if let Some(data) = self.storage.get(stroage_key.as_bytes())? {
            if let Ok(v) = bincode::deserialize(&data) {
                self.series_cache.insert(hash_id, v);
                keys = self.series_cache.get_mut(&hash_id).unwrap();
            }
        }

        if let Some(k) = keys.iter().find(|key| key.id() == sid) {
            return Ok(Some(k.clone()));
        }

        Ok(None)
    }

    pub async fn del_series_info(&mut self, sid: u64) -> IndexResult<()> {
        let (hash_id, _) = utils::split_id(sid);
        self.series_cache.remove(&hash_id);

        let stroage_key = format!("{}{}", SERIES_KEY_PREFIX, hash_id.to_string());
        if let Some(data) = self.storage.get(stroage_key.as_bytes())? {
            if let Ok(keys) = bincode::deserialize::<Vec<SeriesKey>>(&data) {
                if let Some(key) = keys.iter().find(|key| key.id() == sid) {
                    for tag in key.tags() {
                        let key =
                            utils::encode_inverted_index_key(key.table(), &tag.key, &tag.value);
                        if let Some(data) = self.storage.get(&key)? {
                            let mut id_list = utils::decode_series_id_list(&data)?;
                            if let Ok(index) = id_list.binary_search(&sid) {
                                id_list.remove(index);
                            }

                            let data = utils::encode_series_id_list(&id_list);
                            self.storage.set(&key, &data)?;
                        }
                    }

                    let keys: Vec<&SeriesKey> = keys.iter().filter(|k| k.id() != sid).collect();
                    self.storage
                        .set(stroage_key.as_bytes(), &bincode::serialize(&keys).unwrap())?;
                }
            }
        }

        Ok(())
    }

    pub async fn get_series_id_list(&self, tab: &String, tags: &Vec<Tag>) -> IndexResult<Vec<u64>> {
        let mut result: Vec<u64> = vec![];
        if tags.len() == 0 {
            let mut it = self.storage.prefix(format!("{}.", tab).as_bytes());
            loop {
                if let Some(kv) = it.next() {
                    if let Ok(kv) = kv {
                        let id_list = utils::decode_series_id_list(&kv.1)?;
                        result = utils::or_u64(&result, &id_list);
                    } else {
                        return Err(IndexError::IndexStroage {
                            msg: format!("scan prefix: {}", tab),
                        });
                    }
                } else {
                    break;
                }
            }

            return Ok(result);
        }

        let key = utils::encode_inverted_index_key(tab, &tags[0].key, &tags[0].value);
        if let Some(data) = self.storage.get(&key)? {
            result = utils::decode_series_id_list(&data)?;
            if tags.len() == 1 {
                return Ok(result);
            }
        }

        for tag in &tags[1..] {
            let key = utils::encode_inverted_index_key(tab, &tag.key, &tag.value);
            if let Some(data) = self.storage.get(&key)? {
                let id_list = utils::decode_series_id_list(&data)?;
                result = utils::and_u64(&result, &id_list);
            } else {
                return Ok(vec![]);
            }
        }

        Ok(result)
    }
}
