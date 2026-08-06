//! Linux 密钥存储：Secret Service (libsecret via dbus-secret-service)
//!
//! 身份密钥对经 Secret Service 加密存储，不落明文盘。
//! 以 `service` / `account` 作为 item 的检索属性。

use std::collections::HashMap;

use secret_service::{EncryptionType, SecretService};

pub fn store(service: &str, account: &str, data: &[u8]) -> Result<(), String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    collection
        .create_item(account, attributes, data, true)
        .map_err(|e| format!("create item failed: {e}"))?;

    Ok(())
}

pub fn load(service: &str, account: &str) -> Result<Vec<u8>, String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    let items = collection
        .search_items(attributes)
        .map_err(|e| format!("search items failed: {e}"))?;

    let item = items
        .into_iter()
        .next()
        .ok_or_else(|| "no stored credential found".to_string())?;

    item.get_secret()
        .map_err(|e| format!("get secret failed: {e}"))
}

pub fn delete(service: &str, account: &str) -> Result<(), String> {
    let service_api = SecretService::connect(EncryptionType::Dh)
        .map_err(|e| format!("secret service connect failed: {e}"))?;
    let collection = service_api
        .get_default_collection()
        .map_err(|e| format!("get default collection failed: {e}"))?;

    let mut attributes = HashMap::new();
    attributes.insert("service", service);
    attributes.insert("account", account);

    let items = collection
        .search_items(attributes)
        .map_err(|e| format!("search items failed: {e}"))?;

    for item in items {
        item.delete()
            .map_err(|e| format!("delete item failed: {e}"))?;
    }
    Ok(())
}
