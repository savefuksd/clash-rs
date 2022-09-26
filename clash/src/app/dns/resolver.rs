use async_trait::async_trait;
use futures::lock::{Mutex, MutexGuard};
use futures::FutureExt;
use hyper::body::HttpBody;
use log::error;
use rand::prelude::SliceRandom;
use std::borrow::{Borrow, BorrowMut};
use std::cell::{Ref, RefCell};
use std::time::Duration;
use std::{io, net, sync::Arc};
use tokio::time::timeout;
use trust_dns_proto::{op, rr};
use trust_dns_resolver::TokioAsyncResolver;

use crate::dns::ThreadSafeDNSClient;
use crate::{common::trie, dns, Error};

use super::{
    dns_client::{DnsClient, Opts},
    filters::{DomainFilter, FallbackDomainFilter, FallbackIPFilter, GeoIPFilter, IPNetFilter},
    Client, Config, NameServer,
};

static TTL: Duration = Duration::from_secs(60);

/// A implementation of "anti-poisoning" Resolver
/// it can hold multiple clients in different protocols
/// each client can also hold a "default_resolver"
/// in case they need to resolve DoH in domain names etc.  
#[async_trait]
pub trait ClashResolver: Sync + Send {
    async fn resolve(&self, host: &str) -> anyhow::Result<Option<net::IpAddr>>;
    async fn resolve_v4(&self, host: &str) -> anyhow::Result<Option<net::Ipv4Addr>>;
    async fn resolve_v6(&self, host: &str) -> anyhow::Result<Option<net::Ipv6Addr>>;
}

pub struct Resolver {
    ipv6: bool,
    hosts: Option<trie::StringTrie<net::IpAddr>>,
    main: Vec<ThreadSafeDNSClient>,

    fallback: Option<Vec<ThreadSafeDNSClient>>,
    fallback_domain_filters: Option<Vec<Box<dyn FallbackDomainFilter>>>,
    fallback_ip_filters: Option<Vec<Box<dyn FallbackIPFilter>>>,

    lru_cache: Option<lru_time_cache::LruCache<String, op::Message>>,
    policy: Option<trie::StringTrie<Vec<ThreadSafeDNSClient>>>,
}

impl Resolver {
    /// guaranteed to return at least 1 IP address when Ok
    async fn lookup_ip(
        &self,
        host: &str,
        record_type: rr::record_type::RecordType,
    ) -> anyhow::Result<Vec<net::IpAddr>> {
        let mut m = op::Message::new();
        let mut q = op::Query::new();
        q.set_name(
            rr::Name::from_utf8(host)
                .map_err(|x| Error::DNSError(format!("invalid domain: {}", host)))?,
        );
        q.set_query_type(record_type);
        m.add_query(q);

        match self.exchange(m).await {
            Ok(result) => {
                let ip_list = Resolver::ip_list_of_message(&result);
                if !ip_list.is_empty() {
                    Ok(ip_list)
                } else {
                    Err(Error::DNSError("no record".into()).into())
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn exchange(&self, message: op::Message) -> anyhow::Result<op::Message> {
        if let Some(q) = message.query() {
            if let Some(lru) = &self.lru_cache {
                if let Some(cached) = lru.peek(q.to_string().as_str()) {
                    return Ok(cached.clone());
                }
            }
            self.exchange_no_cache(&message).await
        } else {
            Err(Error::DNSError("invalid query".to_string()).into())
        }
    }

    async fn exchange_no_cache(&self, message: &op::Message) -> anyhow::Result<op::Message> {
        let q = message.query().unwrap();

        if Resolver::is_ip_request(q) {
            return self.ip_exchange(message).await;
        }

        if let Some(matched) = self.match_policy(&message) {
            return self.batch_exchange(&matched, message).await;
        }
        return self.batch_exchange(&self.main, message).await;
    }

    fn match_policy(&self, m: &op::Message) -> Option<&Vec<ThreadSafeDNSClient>> {
        if let (Some(fallback), Some(fallback_domain_filters), Some(policy)) =
            (&self.fallback, &self.fallback_domain_filters, &self.policy)
        {
            if let Some(domain) = Resolver::domain_name_of_message(m) {
                return policy.search(&domain).map(|n| n.get_data().unwrap());
            }
        }
        None
    }

    async fn batch_exchange(
        &self,
        clients: &Vec<ThreadSafeDNSClient>,
        message: &op::Message,
    ) -> anyhow::Result<op::Message> {
        // TODO: make this an option

        let mut queries = Vec::new();
        for c in clients {
            // TODO: how to use .map()
            queries.push(async move { c.lock().await.exchange(message).await }.boxed())
        }

        let timeout = tokio::time::sleep(Duration::from_secs(10));

        tokio::select! {
            result = futures::future::select_ok(queries) => match result {
                Ok(r) => Ok(r.0),
                Err(e) => Err(e.into()),
            },
            _ = timeout => Err(Error::DNSError("DNS query timeout".into()).into())
        }
    }

    async fn ip_exchange(&self, message: &op::Message) -> anyhow::Result<op::Message> {
        if let Some(mut matched) = self.match_policy(message) {
            return self.batch_exchange(&mut matched, message).await;
        }

        if self.should_only_query_fallback(message) {
            // self.fallback guaranteed in the above check
            return self
                .batch_exchange(&self.fallback.as_ref().unwrap(), message)
                .await;
        }

        let main_query = self.batch_exchange(&self.main, message);

        if self.fallback.is_none() {
            return main_query.await;
        }

        let fallback_query = self.batch_exchange(&self.fallback.as_ref().unwrap(), message);

        if let Ok(main_result) = main_query.await {
            let ip_list = Resolver::ip_list_of_message(&main_result);
            if !ip_list.is_empty() {
                // TODO: only check 1st?
                if !self.should_ip_fallback(&ip_list[0]) {
                    return Ok(main_result);
                }
            }
        }

        fallback_query.await
    }

    fn should_only_query_fallback(&self, message: &op::Message) -> bool {
        if let (Some(fallback), Some(fallback_domain_filters)) =
            (&self.fallback, &self.fallback_domain_filters)
        {
            if let Some(domain) = Resolver::domain_name_of_message(message) {
                if let Some(filters) = &self.fallback_domain_filters {
                    for f in filters.into_iter() {
                        if f.apply(domain.as_str()) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn should_ip_fallback(&self, ip: &net::IpAddr) -> bool {
        if let Some(filers) = &self.fallback_ip_filters {
            for f in filers.iter() {
                if f.apply(ip) {
                    return true;
                }
            }
        }
        false
    }

    // helpers
    fn is_ip_request(q: &op::Query) -> bool {
        q.query_class() == rr::DNSClass::IN
            && (q.query_type() == rr::RecordType::A || q.query_type() == rr::RecordType::AAAA)
    }

    fn domain_name_of_message(m: &op::Message) -> Option<String> {
        m.query()
            .map(|x| x.name().to_ascii().trim_matches('.').to_owned())
    }

    fn ip_list_of_message(m: &op::Message) -> Vec<net::IpAddr> {
        m.answers()
            .into_iter()
            .filter(|r| {
                r.record_type() == rr::RecordType::A || r.record_type() == rr::RecordType::AAAA
            })
            .map(|r| match r.data() {
                Some(data) => match data {
                    rr::RData::A(v4) => net::IpAddr::V4(*v4),
                    rr::RData::AAAA(v6) => net::IpAddr::V6(*v6),
                    _ => unreachable!("should be only A/AAAA"),
                },
                None => unreachable!("should only be A/AAAA"),
            })
            .collect()
    }
}

#[async_trait]
impl ClashResolver for Resolver {
    async fn resolve(&self, host: &str) -> anyhow::Result<Option<net::IpAddr>> {
        match self.ipv6 {
            true => self
                .resolve_v6(host)
                .await
                .map(|ip| ip.map(|v6| net::IpAddr::from(v6))),
            false => self
                .resolve_v4(host)
                .await
                .map(|ip| ip.map(|v4| net::IpAddr::from(v4))),
        }
    }
    async fn resolve_v4(&self, host: &str) -> anyhow::Result<Option<net::Ipv4Addr>> {
        if let Some(hosts) = &self.hosts {
            if let Some(v) = hosts.search(host) {
                return Ok(v.get_data().map(|v| match v {
                    net::IpAddr::V4(v4) => *v4,
                    _ => unreachable!("invalid IP family"),
                }));
            }
        }

        if let Ok(ip) = host.parse::<net::Ipv4Addr>() {
            return Ok(Some(ip));
        }

        match self.lookup_ip(host, rr::RecordType::A).await {
            Ok(result) => match result.choose(&mut rand::thread_rng()).unwrap() {
                net::IpAddr::V4(v4) => Ok(Some(*v4)),
                _ => unreachable!("invalid IP family"),
            },
            Err(e) => Err(e.into()),
        }
    }
    async fn resolve_v6(&self, host: &str) -> anyhow::Result<Option<net::Ipv6Addr>> {
        if !self.ipv6 {
            return Err(Error::DNSError("ipv6 disabled".into()).into());
        }
        if let Some(hosts) = &self.hosts {
            if let Some(v) = hosts.search(host) {
                return Ok(v.get_data().map(|v| match v {
                    net::IpAddr::V6(v6) => *v6,
                    _ => unreachable!("invalid IP family"),
                }));
            }
        }

        if let Ok(ip) = host.parse::<net::Ipv6Addr>() {
            return Ok(Some(ip));
        }

        match self.lookup_ip(host, rr::RecordType::AAAA).await {
            Ok(result) => match result.choose(&mut rand::thread_rng()).unwrap() {
                net::IpAddr::V6(v6) => Ok(Some(*v6)),
                _ => unreachable!("invalid IP family"),
            },

            Err(e) => Err(e.into()),
        }
    }
}

impl Resolver {
    pub async fn new(cfg: Config) -> Self {
        let default_resolver = Arc::new(Resolver {
            ipv6: false,
            hosts: None,
            main: Resolver::make_clients(cfg.default_nameserver, None).await,
            fallback: None,
            fallback_domain_filters: None,
            fallback_ip_filters: None,
            lru_cache: None,
            policy: None,
        });

        let r = Resolver {
            ipv6: cfg.ipv6,
            main: Resolver::make_clients(cfg.nameserver, Some(default_resolver.clone())).await,
            hosts: cfg.hosts,
            fallback: if cfg.fallback.len() > 0 {
                Some(Resolver::make_clients(cfg.fallback, Some(default_resolver.clone())).await)
            } else {
                None
            },
            fallback_domain_filters: if cfg.fallback_filter.domain.len() > 0 {
                Some(vec![Box::new(DomainFilter::new(
                    cfg.fallback_filter
                        .domain
                        .iter()
                        .map(|x| x.as_str())
                        .collect(),
                )) as Box<dyn FallbackDomainFilter>])
            } else {
                None
            },
            fallback_ip_filters: if cfg.fallback_filter.ip_cidr.is_some()
                || cfg.fallback_filter.geo_ip
            {
                let mut filters = vec![];

                filters.push(Box::new(GeoIPFilter::new(&cfg.fallback_filter.geo_ip_code))
                    as Box<dyn FallbackIPFilter>);

                if let Some(ipcidr) = cfg.fallback_filter.ip_cidr {
                    for subnet in ipcidr {
                        filters
                            .push(Box::new(IPNetFilter::new(subnet)) as Box<dyn FallbackIPFilter>)
                    }
                }

                Some(filters)
            } else {
                None
            },
            lru_cache: Some(lru_time_cache::LruCache::with_expiry_duration_and_capacity(
                TTL, 4096,
            )),
            policy: if cfg.nameserver_policy.len() > 0 {
                let mut p = trie::StringTrie::new();
                for (domain, ns) in cfg.nameserver_policy {
                    p.insert(
                        domain.as_str(),
                        Arc::new(
                            Resolver::make_clients(vec![ns], Some(default_resolver.clone())).await,
                        ),
                    );
                }
                Some(p)
            } else {
                None
            },
        };

        r
    }

    async fn make_clients(
        servers: Vec<NameServer>,
        resolver: Option<Arc<dyn ClashResolver>>,
    ) -> Vec<ThreadSafeDNSClient> {
        let mut rv = Vec::new();

        for s in servers {
            match s.net.as_str() {
                "https" => todo!(),
                "dhcp" => todo!(),
                _ => {
                    let port = s.address.split(":").last().unwrap();
                    let host = s
                        .address
                        .strip_suffix(format!(":{}", port).as_str())
                        .unwrap();

                    match DnsClient::new(Opts {
                        r: resolver.as_ref().map(|x| x.clone()),
                        host: host.to_string(),
                        port: port.parse::<u16>().unwrap(),
                        net: s.net,
                        iface: s.interface.map(|iface| {
                            net::SocketAddr::new(
                                get_if_addrs::get_if_addrs()
                                    .ok()
                                    .expect("failed to lookup local ip")
                                    .into_iter()
                                    .find(|x| x.name == iface)
                                    .map(|x| x.addr.ip())
                                    .expect("no ip address on interface"),
                                0,
                            )
                        }),
                    })
                    .await
                    {
                        Ok(c) => {
                            rv.push(Arc::new(futures::lock::Mutex::new(c)) as ThreadSafeDNSClient)
                        }
                        Err(e) => error!("initializing DNS client: {}", e),
                    }
                }
            }
        }

        rv
    }
}

#[cfg(test)]
mod tests {
    use crate::dns::{ClashResolver, Resolver};
    use crate::{def, dns};
    use std::net;
    use std::path::PathBuf;
    use trust_dns_proto::rr;

    #[tokio::test]
    async fn test_resolve() {
        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("test_data/config.yaml");

        let c = d
            .into_os_string()
            .into_string()
            .unwrap()
            .as_str()
            .parse::<def::Config>()
            .unwrap();
        let r = Resolver::new(c.try_into().unwrap()).await;

        assert!(
            !r.resolve("google.com")
                .await
                .unwrap()
                .unwrap()
                .is_unspecified(),
            "DNS resolution failure"
        );
    }
}
