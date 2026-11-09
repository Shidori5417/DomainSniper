//! NS lookups through the system resolver.

use std::time::Duration;

use hickory_resolver::config::{ResolverConfig, CLOUDFLARE};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{RData, RecordType};
use hickory_resolver::{Resolver, TokioResolver};

use super::{clean_host, CheckError};

pub struct Dns {
    resolver: TokioResolver,
}

impl Dns {
    /// Uses the system resolver config, or public resolvers when it cannot be read.
    pub fn new(timeout: Duration) -> anyhow::Result<Self> {
        let mut builder = Resolver::builder_tokio().unwrap_or_else(|err| {
            tracing::warn!("sistem DNS yapılandırması okunamadı ({err}); Cloudflare kullanılıyor");
            Resolver::builder_with_config(
                ResolverConfig::udp_and_tcp(&CLOUDFLARE),
                TokioRuntimeProvider::default(),
            )
        });
        builder.options_mut().timeout = timeout;
        Ok(Self {
            resolver: builder.build()?,
        })
    }

    /// Sorted, lowercase NS host names. A missing domain or record set yields an empty list.
    pub async fn nameservers(&self, domain: &str) -> Result<Vec<String>, CheckError> {
        // Fully qualified so resolv.conf search domains are never appended.
        let name = format!("{}.", domain.trim_end_matches('.'));
        match self.resolver.lookup(name, RecordType::NS).await {
            Ok(lookup) => Ok(sorted_hosts(lookup.answers().iter().filter_map(
                |r| match &r.data {
                    RData::NS(ns) => Some(ns.0.to_ascii()),
                    _ => None,
                },
            ))),
            Err(err) => classify(&err),
        }
    }
}

fn sorted_hosts(hosts: impl Iterator<Item = String>) -> Vec<String> {
    let mut hosts: Vec<String> = hosts.map(|h| clean_host(&h)).collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// NXDOMAIN / NoData mean "no NS"; anything else is a resolver problem.
fn classify(err: &NetError) -> Result<Vec<String>, CheckError> {
    match err {
        NetError::Dns(DnsError::NoRecordsFound(no))
            if matches!(
                no.response_code,
                ResponseCode::NXDomain | ResponseCode::NoError
            ) =>
        {
            Ok(Vec::new())
        }
        _ => Err(CheckError::Transient(format!("DNS: {err}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_lowercased_stripped_sorted_and_deduped() {
        let hosts = ["NS2.Example.com.", "ns1.example.com.", "ns2.example.com"];
        let got = sorted_hosts(hosts.iter().map(|h| h.to_string()));
        assert_eq!(got, ["ns1.example.com", "ns2.example.com"]);
    }

    #[test]
    fn timeouts_are_transient() {
        assert!(matches!(
            classify(&NetError::Timeout),
            Err(CheckError::Transient(_))
        ));
    }
}
