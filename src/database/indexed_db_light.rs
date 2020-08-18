//! Persistent storage for light-client data based on IndexedDB, the in-brower database.

// TODO: obviously very work-in-progress

#![cfg(feature = "wasm-bindings")]
#![cfg_attr(docsrs, doc(cfg(feature = "wasm-bindings")))]

use crate::header;

use futures::channel::oneshot;
use js_sys::{Array, ArrayBuffer, Uint8Array};
use wasm_bindgen::{prelude::*, JsCast as _};
use web_sys::{DomException, Event, IdbDatabase, IdbTransaction, IdbTransactionMode};

/// An open database.
pub struct Database {
    inner: send_wrapper::SendWrapper<IdbDatabase>,
}

impl Database {
    /// Tries to open the database from the browser environment.
    pub async fn open(db_name: &str) -> Result<Self, OpenError> {
        // TODO: also allow `WorkerGlobalScope`
        let window = web_sys::window().ok_or(OpenError::NoWindow)?;
        let idb_factory = window
            .indexed_db()
            .map_err(OpenError::IndexedDbNotSupported)?
            .unwrap();
        let open_request = idb_factory.open_with_u32(db_name, 1).unwrap();

        // Used to signal when the open request is complete.
        let (tx, rx) = oneshot::channel();

        let on_finish = Closure::once_into_js(move |_: &Event| {
            let _ = tx.send(());
        });
        open_request.set_onsuccess(Some(&on_finish.dyn_ref().unwrap()));
        open_request.set_onerror(Some(&on_finish.dyn_ref().unwrap()));

        let on_upgrade_needed = Closure::once(move |event: &Event| {
            let old_version = {
                let old_version = event
                    .dyn_ref::<web_sys::IdbVersionChangeEvent>()
                    .unwrap()
                    .old_version();
                assert_eq!(old_version.fract(), 0.0);
                assert!(old_version >= 0.0);
                assert!(old_version < u32::max_value() as f64);
                old_version as u32
            };

            let database = event
                .target()
                .unwrap()
                .dyn_into::<web_sys::IdbRequest>()
                .unwrap()
                .result()
                .unwrap()
                .dyn_into::<IdbDatabase>()
                .unwrap();
            create_schema(&database, old_version);
        });
        open_request.set_onupgradeneeded(Some(&on_upgrade_needed.as_ref().dyn_ref().unwrap()));

        // Block until either `onsuccess` or `onerror` happens.
        let _ = rx.await.unwrap();

        // `result()` would return an error if the request wasn't complete yet.
        let result = open_request.result().unwrap();
        match result.dyn_into::<IdbDatabase>() {
            Ok(db) => Ok(Database {
                inner: send_wrapper::SendWrapper::new(db),
            }),
            Err(err) => Err(OpenError::OpenError(err)),
        }
    }

    /// Inserts the given header in the database.
    pub async fn insert_header(&self, scale_encoded_header: &[u8]) -> Result<(), AccessError> {
        let key = {
            let bytes = header::hash_from_scale_encoded_header(scale_encoded_header);
            let hex = hex::encode(&bytes);
            JsValue::from_str(&hex)
        };

        let number = {
            let height = header::decode(&scale_encoded_header).unwrap().number;
            JsValue::from_f64(height as f64)
        };

        let value = {
            let hex = hex::encode(scale_encoded_header);
            JsValue::from_str(&hex)
        };

        let transaction = {
            let obj_stores_list = js_sys::Array::new();
            obj_stores_list.push(&JsValue::from_str("block-headers"));
            obj_stores_list.push(&JsValue::from_str("best-chain"));

            self.inner
                .transaction_with_str_sequence_and_mode(
                    obj_stores_list.as_ref(),
                    IdbTransactionMode::Readwrite,
                )
                .unwrap()
        };

        match transaction
            .object_store("block-headers")
            .unwrap()
            .add_with_key(&value, &key) // Note: the order of parameters is indeed value then key
        {
            Ok(_) => {}
            Err(err) => {
                let err = err.dyn_into::<DomException>().unwrap();
                if err.name() == "ConstraintError" {
                    // Entry already exists in database.
                    return Ok(());
                }
                return Err(AccessError::TransactionError(err));
            }
        }

        // TODO: don't insert if not best; this needs brainstorming because of reorgs
        transaction
            .object_store("best-chain")
            .unwrap()
            .put_with_key(&key, &number) // Note: the order of parameters is indeed value then key
            .unwrap();

        wait_transaction(transaction)
            .await
            .map_err(AccessError::TransactionError)
    }

    /// Reads one value at the given key.
    ///
    /// # Panic
    ///
    /// Panics if the `column_name` is invalid.
    ///
    async fn get(&self, column_name: &str, key: &str) -> Result<Option<String>, AccessError> {
        let transaction = self
            .inner
            .transaction_with_str_and_mode(column_name, IdbTransactionMode::Readonly)
            .unwrap();

        let store = transaction.object_store(column_name).unwrap();
        let query = match store.get(&JsValue::from_str(key)) {
            Ok(r) => r,
            Err(err) => {
                let err = err.dyn_into::<DomException>().unwrap();
                if err.name() == "DataError" {
                    return Ok(None);
                }
                panic!("Unexpected database error: {:?}")
            }
        };

        let (tx, rx) = oneshot::channel();

        // `once_into_js` de-allocates the closure only after it has been called. It is an
        // error to call it multiple times, and if it is not called, it will leak.
        // For this reason, we use the same callback on both success and failure.
        let on_finish = Closure::once_into_js(move |_: &Event| {
            let _ = tx.send(());
        });

        query.set_onsuccess(Some(&on_finish.dyn_ref().unwrap()));
        query.set_onerror(Some(&on_finish.dyn_ref().unwrap()));

        // Block until either `onsuccess` or `onerror` happens.
        let _ = rx.await.unwrap();

        if let Some(result) = query.result().unwrap().as_string() {
            Ok(Some(result))
        } else {
            Err(AccessError::Corrupted(CorruptedError::UnexpectedValueTy))
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        self.inner.close();
    }
}

/// Updates a database to the latest version.
///
/// Called by the `onupgradeneeded` handle of the database.
fn create_schema(database: &IdbDatabase, old_version: u32) {
    if old_version <= 0 {
        // Keys are hex-encoded block hashes, and values are hex-encoded SCALE-encoded block
        // headers.
        database.create_object_store("block-headers").unwrap();

        // Keys are block numbers, and values are hex-encoded block hashes.
        database.create_object_store("best-chain").unwrap();
    }

    // Note: add new versions with something like:
    // if current_version <= N {
    //     database.create_object_store("...").unwrap();
    // }
}

/// Waits for the given transaction to complete.
async fn wait_transaction(transaction: IdbTransaction) -> Result<(), DomException> {
    let (tx, rx) = oneshot::channel();

    let on_finish = Closure::once_into_js(move |_: &Event| {
        let _ = tx.send(());
    });

    transaction.set_oncomplete(Some(&on_finish.dyn_ref().unwrap()));
    transaction.set_onabort(Some(&on_finish.dyn_ref().unwrap()));
    transaction.set_onerror(Some(&on_finish.dyn_ref().unwrap()));

    let _ = rx.await.unwrap();

    match transaction.error() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Error when opening the database.
#[derive(Debug, derive_more::Display)]
pub enum OpenError {
    NoWindow,
    /// IndexedDB is not supported by the environment.
    #[display(fmt = "IndexedDB is not supported by the environment: {:?}", _0)]
    IndexedDbNotSupported(JsValue),
    /// The `IDBOpenDBRequest` produced an error.
    #[display(fmt = "The `IDBOpenDBRequest` produced an error: {:?}", _0)]
    OpenError(JsValue),
}

/// Error accessing the database.
#[derive(Debug, derive_more::Display)]
pub enum AccessError {
    Corrupted(CorruptedError),
    #[display(fmt = "Error while committing transaction: {:?}", _0)]
    TransactionError(DomException),
}

#[derive(Debug, derive_more::Display)]
pub enum CorruptedError {
    UnexpectedValueTy,
}
