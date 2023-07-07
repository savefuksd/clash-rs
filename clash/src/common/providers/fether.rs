use std::{
    fs::{self, metadata},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use tokio::{sync::Mutex, time::Instant};

use crate::common::utils;

use super::{ProviderVehicleType, ThreadSafeProviderVehicle};

struct FetcherInner {
    updated_at: SystemTime,
    hash: [u8; 16],
}

pub(super) struct Fetcher<U, P> {
    name: String,
    interval: Duration,
    vehicle: ThreadSafeProviderVehicle,
    thread_handle: Option<tokio::task::JoinHandle<()>>,
    ticker: Option<tokio::time::Interval>,
    inner: std::sync::Arc<tokio::sync::Mutex<FetcherInner>>,
    parser: Arc<Mutex<P>>,
    on_update: Arc<Mutex<Option<U>>>,
}

impl<T, U, P> Fetcher<U, P>
where
    T: Send + Sync + 'static,
    U: Fn(T) + Send + Sync + 'static,
    P: Fn(&[u8]) -> anyhow::Result<T> + Send + Sync + 'static,
{
    pub(super) fn new(
        name: String,
        interval: Duration,
        vehicle: ThreadSafeProviderVehicle,
        parser: P,
        on_update: Option<U>,
    ) -> Self {
        Self {
            name,
            interval,
            vehicle,
            thread_handle: None,
            ticker: match interval.as_secs() {
                0 => None,
                _ => Some(tokio::time::interval_at(
                    Instant::now() + interval,
                    interval,
                )),
            },
            inner: Arc::new(tokio::sync::Mutex::new(FetcherInner {
                updated_at: SystemTime::UNIX_EPOCH,
                hash: [0; 16],
            })),
            parser: Arc::new(Mutex::new(parser)),
            on_update: Arc::new(Mutex::new(on_update)),
        }
    }
    fn name(&self) -> &str {
        self.name.as_str()
    }

    async fn vehicle_type(&self) -> super::ProviderVehicleType {
        self.vehicle.lock().await.typ()
    }

    async fn initial(&mut self) -> anyhow::Result<T> {
        let mut is_local = false;
        let mut immediately_update = false;

        let vehicle_path = {
            let l = self.vehicle.lock().await;
            l.path().to_owned()
        };

        let mut inner = self.inner.lock().await;

        let content = match metadata(&vehicle_path) {
            Ok(meta) => {
                let content = fs::read(&vehicle_path)?;
                is_local = true;
                inner.updated_at = meta.modified()?;
                immediately_update = SystemTime::now()
                    .duration_since(inner.updated_at)
                    .expect("wrong system clock")
                    > self.interval;
                content
            }
            Err(_) => self.vehicle.lock().await.read().await?,
        };

        let proxies = match (self.parser.lock().await)(&content) {
            Ok(proxies) => proxies,
            Err(e) => {
                if !is_local {
                    return Err(e);
                }
                let content = self.vehicle.lock().await.read().await?;
                (self.parser.lock().await)(&content)?
            }
        };

        if self.vehicle_type().await != ProviderVehicleType::File && !is_local {
            let p = self.vehicle.lock().await.path().to_owned();
            let path = Path::new(p.as_str());
            let prefix = path.parent().unwrap();
            if !prefix.exists() {
                fs::create_dir_all(prefix)?;
            }
            fs::write(self.vehicle.lock().await.path(), &content)?;
        }

        inner.hash = utils::md5(&content)[..16]
            .try_into()
            .expect("md5 must be 16 bytes");

        drop(inner);

        if let Some(ticker) = self.ticker.take() {
            self.pull_loop(immediately_update, ticker);
        }

        Ok(proxies)
    }

    async fn update(&self) -> anyhow::Result<(T, bool)> {
        Fetcher::<U, P>::update_inner(
            self.inner.clone(),
            self.vehicle.clone(),
            self.parser.clone(),
        )
        .await
    }

    async fn update_inner(
        inner: Arc<Mutex<FetcherInner>>,
        vehicle: ThreadSafeProviderVehicle,
        parser: Arc<Mutex<P>>,
    ) -> anyhow::Result<(T, bool)> {
        let mut this = inner.blocking_lock();
        let content = vehicle.lock().await.read().await?;
        let proxies = (parser.lock().await)(&content)?;

        let now = SystemTime::now();
        let hash = utils::md5(&content)[..16]
            .try_into()
            .expect("md5 must be 16 bytes");

        if hash == this.hash {
            this.updated_at = now;
            filetime::set_file_times(vehicle.lock().await.path(), now.into(), now.into())?;
            return Ok((proxies, false));
        }

        let proxies = (parser.lock().await)(&content)?;

        if vehicle.lock().await.typ() != ProviderVehicleType::File {
            let p = vehicle.lock().await.path().to_owned();
            let path = Path::new(p.as_str());
            let prefix = path.parent().unwrap();
            if !prefix.exists() {
                fs::create_dir_all(prefix)?;
            }

            fs::write(vehicle.lock().await.path(), &content)?;
            return Ok((proxies, false));
        }

        this.hash = hash;
        this.updated_at = now;

        Ok((proxies, false))
    }

    fn destroy(&mut self) {
        if let Some(handle) = self.thread_handle.take() {
            handle.abort();
        }
    }

    fn pull_loop(&mut self, immediately_update: bool, mut ticker: tokio::time::Interval) {
        let inner = self.inner.clone();
        let vehicle = self.vehicle.clone();
        let parser = self.parser.clone();
        let on_update = self.on_update.clone();
        let name = self.name.clone();
        let mut fire_immediately = immediately_update;

        self.thread_handle = Some(tokio::spawn(async move {
            loop {
                let inner = inner.clone();
                let vehicle = vehicle.clone();
                let parser = parser.clone();
                let name = name.clone();
                let on_update = on_update.clone();
                let update = || async move {
                    let (elm, same) =
                        match Fetcher::<U, P>::update_inner(inner, vehicle, parser).await {
                            Ok((elm, same)) => (elm, same),
                            Err(e) => {
                                tracing::error!("{} update failed: {}", &name, e);
                                return;
                            }
                        };

                    if same {
                        tracing::info!("{} no update", &name);
                        return;
                    }

                    tracing::info!("{} updated", &name);

                    let on_update = on_update.blocking_lock().take();
                    if let Some(on_update) = on_update {
                        on_update(elm)
                    }
                };

                ticker.tick().await;
                if fire_immediately {
                    update().await;
                } else {
                    fire_immediately = false;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicU16},
            Arc, Barrier,
        },
        time::Duration,
    };

    use tokio::sync::Mutex;

    use crate::common::providers::{MockProviderVehicle, ProviderVehicleType};

    use super::Fetcher;

    #[tokio::test]
    async fn test_fetcher() {
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();

        let parser_called = Arc::new(AtomicU16::new(0));
        let updater_called = Arc::new(AtomicU16::new(0));

        let parser_called_clone = parser_called.clone();
        let updater_called_clone = updater_called.clone();

        let mut mock_vehicle = MockProviderVehicle::new();
        mock_vehicle
            .expect_path()
            .return_const("/tmp/mock_provider_vehicle".to_owned());
        mock_vehicle.expect_read().returning(|| {
            let mut v = Vec::new();
            v.extend_from_slice(&[1, 2, 3]);
            Ok(v)
        });
        mock_vehicle
            .expect_typ()
            .return_const(ProviderVehicleType::File);

        let p = move |i: &[u8]| -> anyhow::Result<String> {
            assert_eq!(i, vec![1, 2, 3]);
            parser_called_clone
                .clone()
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("parsed".to_owned())
        };
        let o = move |input: String| -> () {
            assert_eq!(input, "parsed".to_owned());
            updater_called_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            barrier_clone.wait();
        };
        let mut f = Fetcher::new(
            "test_fetcher".to_string(),
            Duration::from_secs(1),
            Arc::new(Mutex::new(mock_vehicle)),
            p,
            Some(o),
        );

        let _ = f.initial().await;

        barrier.wait();

        assert_eq!(parser_called.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(updater_called.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
