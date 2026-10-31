//! LocalSend v2.2 的线上形状。
//!
//! 只保留 OrbDrop 需要的那一面：发现/注册、上传准备、上传、取消。字段名按协议用
//! camelCase；未知字段一律忽略（serde 默认），所以对端版本比我们新也不会 400。
//!
//! **协议版本**固定报 `2.2`：当前 LocalSend（v2.x）只认 major.minor。未来 v3 是另一套
//! （WebRTC/信令），不属于这里。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "2.2";

/// `register` / `prepare-upload.info` 的形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    /// 证书 DER 的 SHA-256（大写 hex）。HTTPS 模式下对端以 TLS 握手证书为准，
    /// 这个字段只用于 HTTP/旧客户端；我们只支持 HTTPS，但仍原样回一份。
    pub fingerprint: String,
    pub port: u16,
    pub protocol: String,
    #[serde(default)]
    pub download: bool,
}

/// `GET /api/localsend/v2/info` 的形状（没有 port/protocol）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoResponse {
    pub alias: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    pub fingerprint: String,
    #[serde(default)]
    pub download: bool,
}

/// `POST /api/localsend/v2/register` 的回包形状（与 info 相同）。
pub type RegisterResponse = InfoResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed: Option<String>,
}

/// 一个待传文件。`file_type` 在协议上是 MIME；LocalSend 的文本消息是
/// `text/plain` 且正文塞在 `preview` 里（见 `server::is_text_message`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDto {
    pub id: String,
    pub file_name: String,
    pub size: u64,
    #[serde(default = "default_file_type")]
    pub file_type: String,
    #[serde(default, alias = "hash", skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FileMetadata>,
}

fn default_file_type() -> String {
    "application/octet-stream".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadRequest {
    pub info: DeviceInfo,
    /// file id → 元数据。id 是对端自己生成的，只当不透明键用，绝不作为路径。
    #[serde(default)]
    pub files: HashMap<String, FileDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareUploadResponse {
    pub session_id: String,
    /// 只包含接收方接受的文件；没列出来的文件对端不会上传。
    pub files: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_local_send_prepare_payload() {
        let json = r#"{
          "info": {
            "alias": "Nice Orange",
            "version": "2.2",
            "deviceModel": "Samsung",
            "deviceType": "mobile",
            "fingerprint": "ABC",
            "port": 53317,
            "protocol": "https",
            "download": false
          },
          "files": {
            "f1": {
              "id": "f1",
              "fileName": "我的 文件.txt",
              "size": 12,
              "fileType": "text/plain",
              "preview": "你好",
              "sha256": null
            }
          }
        }"#;
        let req: PrepareUploadRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.info.alias, "Nice Orange");
        assert_eq!(req.files["f1"].file_name, "我的 文件.txt");
        assert_eq!(req.files["f1"].preview.as_deref(), Some("你好"));
    }

    #[test]
    fn missing_optional_fields_are_accepted() {
        let json = r#"{
          "info": {"alias":"A","version":"2.0","fingerprint":"x","port":1,"protocol":"https"},
          "files": {"f": {"id":"f","fileName":"a.bin","size":1}}
        }"#;
        let req: PrepareUploadRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.files["f"].file_type, "application/octet-stream");
    }
}