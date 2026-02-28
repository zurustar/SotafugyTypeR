use std::net::SocketAddr;
use std::time::Instant;

use dashmap::DashMap;

/// Contact information stored in the Location Service.
#[derive(Debug, Clone)]
pub struct ContactInfo {
    pub contact_uri: String,
    pub address: SocketAddr,
    pub expires: Instant,
}

/// In-memory Location Service for REGISTER/INVITE routing.
/// Maps AOR (Address of Record) to ContactInfo.
pub struct LocationService {
    registrations: DashMap<String, ContactInfo>,
}

impl LocationService {
    /// Create a new empty LocationService.
    pub fn new() -> Self {
        Self {
            registrations: DashMap::new(),
        }
    }

    /// Register a contact for the given AOR.
    pub fn register(&self, aor: &str, contact: ContactInfo) {
        self.registrations.insert(aor.to_string(), contact);
    }

    /// Lookup a contact by AOR. Returns None if not registered.
    pub fn lookup(&self, aor: &str) -> Option<ContactInfo> {
        self.registrations.get(aor).map(|entry| entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    #[test]
    fn test_location_service_register_and_lookup() {
        let ls = LocationService::new();
        let contact = ContactInfo {
            contact_uri: "sip:alice@10.0.0.1:5060".to_string(),
            address: "10.0.0.1:5060".parse().unwrap(),
            expires: Instant::now() + Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact);

        let result = ls.lookup("sip:alice@example.com");
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.contact_uri, "sip:alice@10.0.0.1:5060");
        assert_eq!(info.address, "10.0.0.1:5060".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn test_location_service_lookup_unregistered_returns_none() {
        let ls = LocationService::new();
        assert!(ls.lookup("sip:unknown@example.com").is_none());
    }

    #[test]
    fn test_location_service_register_overwrites_previous() {
        let ls = LocationService::new();
        let contact1 = ContactInfo {
            contact_uri: "sip:alice@10.0.0.1:5060".to_string(),
            address: "10.0.0.1:5060".parse().unwrap(),
            expires: Instant::now() + Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact1);

        let contact2 = ContactInfo {
            contact_uri: "sip:alice@10.0.0.2:5060".to_string(),
            address: "10.0.0.2:5060".parse().unwrap(),
            expires: Instant::now() + Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact2);

        let result = ls.lookup("sip:alice@example.com").unwrap();
        assert_eq!(result.contact_uri, "sip:alice@10.0.0.2:5060");
    }
}
