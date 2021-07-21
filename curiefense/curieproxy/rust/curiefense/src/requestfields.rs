use core::iter::FromIterator;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map, HashMap};

/// a newtype for user supplied data that can collide
/// more or less like a HashMap, but concatenates entries with a separator on insert
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestField(pub HashMap<String, String>);

impl Default for RequestField {
    fn default() -> Self {
        RequestField(HashMap::new())
    }
}

impl RequestField {
    fn base_add(&mut self, key: String, value: String) {
        self.0
            .entry(key)
            .and_modify(|v| {
                v.push(' ');
                v.push_str(&value);
            })
            .or_insert(value);
    }

    pub fn add(&mut self, key: String, value: String) {
        // try to insert each value as its decoded base64 version, if it makes sense
        if !value.is_empty() {
            if let Ok(b64decoded) = base64::decode(value.as_bytes()) {
                if let Ok(b64value) = String::from_utf8(b64decoded) {
                    let nkey = key.clone() + "_base64";
                    self.base_add(nkey, b64value);
                }
            }
        }
        self.base_add(key, value);
    }

    pub fn get(&self, k: &str) -> Option<&String> {
        self.0.get(k)
    }

    pub fn get_str(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> hash_map::Iter<'_, String, String> {
        self.0.iter()
    }
}

impl FromIterator<(String, String)> for RequestField {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        let mut out = RequestField::default();
        for (k, v) in iter {
            out.add(k, v);
        }
        out
    }
}
