use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::Uri;

use crate::package_json::{self, Reference};
use crate::workspace::Buffer;

#[derive(Debug, Clone)]
pub struct Document {
    pub uri: Uri,
    pub path: PathBuf,
    pub version: i32,
    pub text: Arc<str>,
    pub references: Arc<[Reference]>,
}

#[derive(Default)]
pub struct DocumentStore {
    docs: Mutex<HashMap<Uri, Document>>,
}

impl DocumentStore {
    pub fn upsert_if_newer(
        &self,
        uri: Uri,
        path: PathBuf,
        version: i32,
        text: String,
    ) -> Option<Document> {
        let mut docs = self.lock();
        if docs.get(&uri).is_some_and(|doc| doc.version > version) {
            return None;
        }
        let installed = path
            .components()
            .any(|part| part.as_os_str() == "node_modules");
        let document = Document {
            references: if installed {
                Arc::from([])
            } else {
                package_json::references(&text).into()
            },
            uri: uri.clone(),
            path,
            version,
            text: text.into(),
        };
        docs.insert(uri, document.clone());
        Some(document)
    }

    pub fn remove(&self, uri: &Uri) -> Option<Document> {
        self.lock().remove(uri)
    }

    pub fn get(&self, uri: &Uri) -> Option<Document> {
        self.lock().get(uri).cloned()
    }

    pub fn all(&self) -> Vec<Document> {
        self.lock().values().cloned().collect()
    }

    pub fn buffer(&self, path: &Path) -> Option<Buffer> {
        self.lock()
            .values()
            .find(|doc| doc.path == path)
            .map(|doc| Buffer {
                version: doc.version,
                text: doc.text.clone(),
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uri, Document>> {
        self.docs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn uri() -> Uri {
        Uri::from_str("file:///ws/package.json").unwrap()
    }

    #[test]
    fn stale_versions_are_dropped() {
        let store = DocumentStore::default();
        let path = PathBuf::from("/ws/package.json");
        store
            .upsert_if_newer(uri(), path.clone(), 1, "{}".into())
            .unwrap();
        store
            .upsert_if_newer(
                uri(),
                path.clone(),
                3,
                r#"{"dependencies":{"a":"catalog:"}}"#.into(),
            )
            .unwrap();
        assert!(
            store
                .upsert_if_newer(uri(), path.clone(), 2, "{}".into())
                .is_none()
        );
        let doc = store.get(&uri()).unwrap();
        assert_eq!(doc.version, 3);
        assert_eq!(doc.references.len(), 1);
        assert_eq!(store.buffer(&path).unwrap().version, 3);
        assert!(store.remove(&uri()).is_some());
        assert!(store.buffer(&path).is_none());
    }
}
