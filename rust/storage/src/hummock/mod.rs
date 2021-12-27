//! Hummock is the state store of the streaming system.

use std::ops::RangeBounds;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use num_traits::ToPrimitive;

mod table;
use prometheus::Registry;
pub use table::*;
mod cloud;
mod compactor;
mod error;
mod iterator;
mod key;
mod key_range;
mod level_handler;
mod mon;
mod snapshot;
mod state_store;
mod utils;
mod value;
mod version_cmp;
mod version_manager;

use cloud::gen_remote_table;
use compactor::Compactor;
pub use error::*;
use parking_lot::Mutex as PLMutex;
use risingwave_pb::hummock::checksum::Algorithm as ChecksumAlg;
use tokio::select;
use tokio::sync::mpsc;
use value::*;

use self::iterator::UserKeyIterator;
use self::key::{key_with_ts, user_key, FullKey};
use self::mon::HummockStats;
use self::snapshot::HummockSnapshot;
pub use self::state_store::*;
use self::version_manager::VersionManager;
use crate::object::ObjectStore;

pub static REMOTE_DIR: &str = "/test/";

#[derive(Default, Debug, Clone)]
pub struct HummockOptions {
    /// target size of the table
    pub table_size: u32,
    /// size of each block in bytes in SST
    pub block_size: u32,
    /// false positive probability of Bloom filter
    pub bloom_false_positive: f64,
    /// remote directory for storing data and metadata objects
    pub remote_dir: String,
    /// checksum algorithm
    pub checksum_algo: ChecksumAlg,
    /// statistics enabled
    pub stats_enabled: bool,
}

impl HummockOptions {
    pub fn default_for_test() -> Self {
        Self {
            table_size: 256 * (1 << 20),
            block_size: 64 * (1 << 10),
            bloom_false_positive: 0.1,
            remote_dir: "hummock_001".to_string(),
            checksum_algo: ChecksumAlg::Crc32c,
            stats_enabled: true,
        }
    }
}

/// Hummock is the state store backend.
#[derive(Clone)]
pub struct HummockStorage {
    options: Arc<HummockOptions>,
    unique_id: Arc<AtomicU64>,
    version_manager: Arc<VersionManager>,
    obj_client: Arc<dyn ObjectStore>,

    /// Notify the compactor to compact after every write_batch().
    tx: mpsc::UnboundedSender<()>,

    /// Receiver of the compactor.
    rx: Arc<PLMutex<Option<mpsc::UnboundedReceiver<()>>>>,

    /// Statistics.
    stats: Option<Arc<HummockStats>>,
}

impl HummockStorage {
    pub fn new(
        obj_client: Arc<dyn ObjectStore>,
        options: HummockOptions,
        stats_registry: Option<Arc<Registry>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut stats = None;

        if options.stats_enabled {
            if let Some(registry) = stats_registry {
                stats = Some(Arc::new(HummockStats::new(&registry)));
            } else {
                stats = Some(Arc::new(HummockStats::new(prometheus::default_registry())));
            }
        }

        Self {
            options: Arc::new(options),
            unique_id: Arc::new(AtomicU64::new(0)),
            version_manager: Arc::new(VersionManager::new()),
            obj_client,
            tx,
            rx: Arc::new(PLMutex::new(Some(rx))),
            stats,
        }
    }

    fn get_snapshot(&self) -> HummockSnapshot {
        HummockSnapshot::new(self.version_manager.clone())
    }

    pub fn get_stats_ref(&self) -> Option<Arc<HummockStats>> {
        self.stats.clone()
    }

    pub fn get_options(&self) -> Arc<HummockOptions> {
        self.options.clone()
    }

    /// Get the latest value of a specified `key`.
    ///
    /// If `Ok(Some())` is returned, the key is found. If `Ok(None)` is returned,
    /// the key is not found. If `Err()` is returned, the searching for the key
    /// failed due to other non-EOF errors.
    pub async fn get(&self, key: &[u8]) -> HummockResult<Option<Vec<u8>>> {
        self.get_snapshot().get(key).await
    }

    /// Return an iterator that scan from the begin key to the end key
    pub async fn range_scan<R, B>(&self, key_range: R) -> HummockResult<UserKeyIterator>
    where
        R: RangeBounds<B>,
        B: AsRef<[u8]>,
    {
        self.get_snapshot().range_scan(key_range).await
    }

    /// Write batch to storage. The batch should be:
    /// * Ordered. KV pairs will be directly written to the table, so it must be ordered.
    /// * Locally unique. There should not be two or more operations on the same key in one write
    ///   batch.
    /// * Globally unique. The streaming operators should ensure that different operators won't
    ///   operate on the same key. The operator operating on one keyspace should always wait for all
    ///   changes to be committed before reading and writing new keys to the engine. That is because
    ///   that the table with lower epoch might be committed after a table with higher epoch has
    ///   been committed. If such case happens, the outcome is non-predictable.
    pub async fn write_batch(
        &self,
        kv_pairs: impl Iterator<Item = (Vec<u8>, HummockValue<Vec<u8>>)>,
    ) -> HummockResult<()> {
        let get_builder = |options: &HummockOptions| {
            TableBuilder::new(TableBuilderOptions {
                table_capacity: options.table_size,
                block_size: options.block_size,
                bloom_false_positive: options.bloom_false_positive,
                checksum_algo: options.checksum_algo,
            })
        };

        let mut table_builder = get_builder(&self.options);
        let table_id = self.unique_id.fetch_add(1, Ordering::SeqCst);
        for (k, v) in kv_pairs {
            // do not allow empty key
            assert!(!k.is_empty());

            let k = key_with_ts(k, table_id);
            table_builder.add(k.as_slice(), v);
        }

        if table_builder.is_empty() {
            return Ok(());
        }

        // Producing only one table regardless of capacity for now.
        // TODO: update kv pairs to multi tables when size of the kv pairs is larger than
        // TODO: the capacity of a single table.
        let (blocks, meta) = table_builder.finish();
        let remote_dir = Some(self.options.remote_dir.as_str());
        let block_len = blocks.len();
        let table =
            gen_remote_table(self.obj_client.clone(), table_id, blocks, meta, remote_dir).await?;

        self.version_manager.add_l0_sst(table).await?;

        // Update statistics if needed.
        if self.options.stats_enabled {
            self.stats
                .clone()
                .unwrap()
                .put_bytes
                .inc_by(block_len.to_u64().unwrap());
        }

        // TODO: should we use unwrap() ?
        self.tx.send(()).unwrap();

        Ok(())
    }

    fn get_builder(options: &HummockOptions) -> TableBuilder {
        // TODO: avoid repeating code in write_batch()
        // TODO: use different option values (especially table_size) for compaction
        TableBuilder::new(TableBuilderOptions {
            table_capacity: options.table_size,
            block_size: options.block_size,
            bloom_false_positive: options.bloom_false_positive,
            checksum_algo: options.checksum_algo,
        })
    }

    pub async fn start_compactor(
        self: &Arc<Self>,
        mut stop: mpsc::UnboundedReceiver<()>,
    ) -> HummockResult<()> {
        let mut compact_notifier = self.rx.lock().take().unwrap();
        loop {
            select! {
                Some(_) = compact_notifier.recv() => Compactor::compact(self).await?,
                Some(_) = stop.recv() => break
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;

    use bytes::Bytes;
    use hyper::body::HttpBody;
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Client, Request, Response, Server};
    use prometheus::{Encoder, Registry, TextEncoder};

    use super::iterator::UserKeyIterator;
    use super::{HummockOptions, HummockStorage};
    use crate::object::InMemObjectStore;

    async fn prometheus_service(
        _req: Request<Body>,
        registry: Arc<Registry>,
    ) -> Result<Response<Body>, hyper::Error> {
        let encoder = TextEncoder::new();
        let mut buffer = vec![];
        let mf = registry.gather();
        encoder.encode(&mf, &mut buffer).unwrap();
        let response = Response::builder()
            .header(hyper::header::CONTENT_TYPE, encoder.format_type())
            .body(Body::from(buffer))
            .unwrap();

        Ok(response)
    }

    #[tokio::test]
    async fn test_prometheus_endpoint() {
        let hummock_options = HummockOptions::default_for_test();
        let host_addr = "127.0.0.1:1222";
        let prom_registry = Some(Arc::new(Registry::new()));
        let hummock_storage = HummockStorage::new(
            Arc::new(InMemObjectStore::new()),
            hummock_options,
            prom_registry.clone(),
        );
        let anchor = Bytes::from("aa");
        let mut batch1 = vec![
            (anchor.clone(), Some(Bytes::from("111"))),
            (Bytes::from("bb"), Some(Bytes::from("222"))),
        ];
        batch1.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
        hummock_storage
            .write_batch(
                batch1
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.map(|x| x.to_vec()).into())),
            )
            .await
            .unwrap();

        let notifier = Arc::new(tokio::sync::Notify::new());
        let notifiee = notifier.clone();
        let registry = prom_registry.unwrap();

        let make_svc = make_service_fn(move |_| {
            let registry = registry.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    let registry = registry.clone();
                    async move { prometheus_service(req, registry).await }
                }))
            }
        });

        let server = Server::bind(&host_addr.parse().unwrap()).serve(make_svc);

        tokio::spawn(async move {
            notifier.notify_one();
            if let Err(err) = server.await {
                eprintln!("server error: {}", err);
            }
        });

        notifiee.notified().await;
        let client = Client::new();
        let uri = "http://127.0.0.1:1222/metrics".parse().unwrap();
        let mut response = client.get(uri).await.unwrap();

        let mut web_page: Vec<u8> = Vec::new();
        while let Some(next) = response.data().await {
            let chunk = next.unwrap();
            web_page.append(&mut chunk.to_vec());
        }

        let s = String::from_utf8_lossy(&web_page);
        assert!(s.contains("hummock_put_bytes"));
        assert!(!s.contains("hummock_pt_byts"));
    }

    #[tokio::test]
    async fn test_basic() {
        let hummock_storage = HummockStorage::new(
            Arc::new(InMemObjectStore::new()),
            HummockOptions::default_for_test(),
            None,
        );
        let anchor = Bytes::from("aa");

        // First batch inserts the anchor and others.
        let mut batch1 = vec![
            (anchor.clone(), Some(Bytes::from("111"))),
            (Bytes::from("bb"), Some(Bytes::from("222"))),
        ];

        // Make sure the batch is sorted.
        batch1.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

        // Second batch modifies the anchor.
        let mut batch2 = vec![
            (Bytes::from("cc"), Some(Bytes::from("333"))),
            (anchor.clone(), Some(Bytes::from("111111"))),
        ];

        // Make sure the batch is sorted.
        batch2.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

        // Third batch deletes the anchor
        let mut batch3 = vec![
            (Bytes::from("dd"), Some(Bytes::from("444"))),
            (Bytes::from("ee"), Some(Bytes::from("555"))),
            (anchor.clone(), None),
        ];

        // Make sure the batch is sorted.
        batch3.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

        // Write first batch.
        hummock_storage
            .write_batch(
                batch1
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.map(|x| x.to_vec()).into())),
            )
            .await
            .unwrap();

        let snapshot1 = hummock_storage.get_snapshot();

        // Get the value after flushing to remote.
        let value = snapshot1.get(&anchor).await.unwrap().unwrap();
        assert_eq!(Bytes::from(value), Bytes::from("111"));

        // Test looking for a nonexistent key. `next()` would return the next key.
        let value = snapshot1.get(&Bytes::from("ab")).await.unwrap();
        assert_eq!(value, None);

        // Write second batch.
        hummock_storage
            .write_batch(
                batch2
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.map(|x| x.to_vec()).into())),
            )
            .await
            .unwrap();

        let snapshot2 = hummock_storage.get_snapshot();

        // Get the value after flushing to remote.
        let value = snapshot2.get(&anchor).await.unwrap().unwrap();
        assert_eq!(Bytes::from(value), Bytes::from("111111"));

        // Write third batch.
        hummock_storage
            .write_batch(
                batch3
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.map(|x| x.to_vec()).into())),
            )
            .await
            .unwrap();

        let snapshot3 = hummock_storage.get_snapshot();

        // Get the value after flushing to remote.
        let value = snapshot3.get(&anchor).await.unwrap();
        assert_eq!(value, None);

        // Get non-existent maximum key.
        let value = snapshot3.get(&Bytes::from("ff")).await.unwrap();
        assert_eq!(value, None);

        // write aa bb
        let mut iter = snapshot1.range_scan(..=b"ee".to_vec()).await.unwrap();
        iter.rewind().await.unwrap();
        let len = count_iter(&mut iter).await;
        assert_eq!(len, 2);

        // Get the anchor value at the first snapshot
        let value = snapshot1.get(&anchor).await.unwrap().unwrap();
        assert_eq!(Bytes::from(value), Bytes::from("111"));

        // drop snapshot 1
        drop(snapshot1);

        // Get the anchor value at the second snapshot
        let value = snapshot2.get(&anchor).await.unwrap().unwrap();
        assert_eq!(Bytes::from(value), Bytes::from("111111"));
        // update aa, write cc
        let mut iter = snapshot2.range_scan(..=b"ee".to_vec()).await.unwrap();
        iter.rewind().await.unwrap();
        let len = count_iter(&mut iter).await;
        assert_eq!(len, 3);

        // drop snapshot 2
        drop(snapshot2);

        // delete aa, write dd,ee
        let mut iter = snapshot3.range_scan(..=b"ee".to_vec()).await.unwrap();
        iter.rewind().await.unwrap();
        let len = count_iter(&mut iter).await;
        assert_eq!(len, 4);
    }
    async fn count_iter(iter: &mut UserKeyIterator) -> usize {
        let mut c: usize = 0;
        while iter.is_valid() {
            c += 1;
            iter.next().await.unwrap();
        }
        c
    }
}
