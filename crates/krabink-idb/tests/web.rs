#![cfg(target_arch = "wasm32")]

use krabink_core::{DocKey, Flush, StoreImage, StoreOp};
use krabink_idb::{Db, Ns};
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

const A: DocKey = DocKey::from_u128(0xa);
const B: DocKey = DocKey::from_u128(u128::MAX);

fn append(key: DocKey, seq: u64, bytes: &[u8]) -> StoreOp {
    StoreOp::Append {
        key,
        seq,
        bytes: bytes.to_vec(),
        flush: Flush::Eventual,
    }
}

async fn fresh(name: &str) -> Db {
    Db::delete_named(name).await.unwrap();
    Db::open_named(name).await.unwrap()
}

#[wasm_bindgen_test]
async fn ops_survive_reopen() {
    const NAME: &str = "krabink-test-reopen";
    let db = fresh(NAME).await;
    let sink = db.sink(Ns::App);
    for (seq, bytes) in [b"a0", b"a1", b"a2"].into_iter().enumerate() {
        sink.apply(append(A, seq as u64, bytes));
    }
    sink.apply(StoreOp::Checkpoint {
        key: A,
        snapshot: b"snap".to_vec(),
    });
    sink.apply(append(A, 0, b"a3"));
    sink.apply(append(B, 0, b"b0"));
    db.kv_put("device", b"dev".to_vec());
    db.flush().await.unwrap();
    assert_eq!(db.health().last_error(), None);
    drop((sink, db));

    let db = Db::open_named(NAME).await.unwrap();
    assert_eq!(
        db.preload(Ns::App).await.unwrap(),
        StoreImage {
            snapshots: vec![(A, b"snap".to_vec())],
            updates: vec![(A, 0, b"a3".to_vec()), (B, 0, b"b0".to_vec())],
        }
    );
    assert_eq!(db.kv_get("device").await.unwrap(), Some(b"dev".to_vec()));
    assert_eq!(db.kv_get("missing").await.unwrap(), None);
    assert_eq!(db.preload(Ns::Node).await.unwrap(), StoreImage::default());
}

#[wasm_bindgen_test]
async fn append_after_checkpoint_survives_across_batches() {
    let db = fresh("krabink-test-order").await;
    let sink = db.sink(Ns::Node);
    sink.apply(append(A, 0, b"old"));
    db.flush().await.unwrap();
    // Checkpoint and the next append land in one batch; the range delete
    // must not swallow the put queued after it.
    sink.apply(StoreOp::Checkpoint {
        key: A,
        snapshot: b"snap".to_vec(),
    });
    sink.apply(append(A, 0, b"new"));
    db.flush().await.unwrap();
    // And across batches.
    sink.apply(StoreOp::Checkpoint {
        key: B,
        snapshot: b"b".to_vec(),
    });
    db.flush().await.unwrap();
    sink.apply(append(B, 0, b"b0"));
    db.flush().await.unwrap();

    assert_eq!(
        db.preload(Ns::Node).await.unwrap(),
        StoreImage {
            snapshots: vec![(A, b"snap".to_vec()), (B, b"b".to_vec())],
            updates: vec![(A, 0, b"new".to_vec()), (B, 0, b"b0".to_vec())],
        }
    );
    assert_eq!(db.preload(Ns::App).await.unwrap(), StoreImage::default());
}
