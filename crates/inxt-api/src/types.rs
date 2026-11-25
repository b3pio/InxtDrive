use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// Sizes arrive as numbers, numeric strings or null depending on the endpoint.
pub fn de_u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        N(u64),
        F(f64),
        S(String),
        Null(()),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        Some(Raw::N(n)) => n,
        Some(Raw::F(f)) if f >= 0.0 => f as u64,
        Some(Raw::S(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

pub fn de_time_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    Ok(s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|t| t.with_timezone(&Utc))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    Exists,
    Trashed,
    Deleted,
}

impl ItemStatus {
    fn parse(s: Option<&str>, deleted: bool, removed: bool) -> Self {
        match s {
            Some("TRASHED") => ItemStatus::Trashed,
            Some("DELETED") => ItemStatus::Deleted,
            Some(_) => ItemStatus::Exists,
            None if removed => ItemStatus::Deleted,
            None if deleted => ItemStatus::Trashed,
            None => ItemStatus::Exists,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDto {
    pub uuid: String,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub plain_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "type")]
    pub file_type: Option<String>,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub size: u64,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub folder_uuid: Option<String>,
    #[serde(default)]
    pub encrypt_version: Option<String>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub creation_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub modification_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub removed: bool,
}

impl FileDto {
    pub fn status(&self) -> ItemStatus {
        ItemStatus::parse(self.status.as_deref(), self.deleted, self.removed)
    }

    /// Name as every Drive client renders it: `plainName` plus `.type`.
    pub fn display_name(&self) -> String {
        let base = self
            .plain_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.uuid.clone());
        match self.file_type.as_deref() {
            Some(t) if !t.is_empty() => format!("{base}.{t}"),
            _ => base,
        }
    }

    pub fn mtime(&self) -> Option<DateTime<Utc>> {
        self.modification_time.or(self.updated_at).or(self.created_at)
    }

    pub fn ctime(&self) -> Option<DateTime<Utc>> {
        self.creation_time.or(self.created_at)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDto {
    pub uuid: String,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub plain_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub creation_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub modification_time: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "de_time_opt")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub removed: bool,
}

impl FolderDto {
    pub fn status(&self) -> ItemStatus {
        ItemStatus::parse(self.status.as_deref(), self.deleted, self.removed)
    }

    pub fn display_name(&self) -> String {
        self.plain_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.uuid.clone())
    }

    pub fn mtime(&self) -> Option<DateTime<Utc>> {
        self.modification_time.or(self.updated_at).or(self.created_at)
    }

    pub fn ctime(&self) -> Option<DateTime<Utc>> {
        self.creation_time.or(self.created_at)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub mnemonic: String,
    #[serde(default)]
    pub root_folder_id: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub bridge_user: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub new_token: String,
    pub user: UserDto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityDetails {
    #[serde(rename = "sKey")]
    pub s_key: String,
    #[serde(default)]
    pub tfa: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginAccessResponse {
    pub new_token: String,
    pub user: UserDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderPage {
    #[serde(default)]
    pub folders: Vec<FolderDto>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilePage {
    #[serde(default)]
    pub files: Vec<FileDto>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub drive: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub max_space_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFileRequest {
    pub bucket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    pub encrypt_version: &'static str,
    pub folder_uuid: String,
    pub size: u64,
    pub plain_name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    pub creation_time: DateTime<Utc>,
    pub date: DateTime<Utc>,
    pub modification_time: DateTime<Utc>,
}

/// Bridge metadata of an encrypted blob: `/buckets/{b}/files/{id}/info`.
#[derive(Debug, Clone, Deserialize)]
pub struct BucketFileInfo {
    #[serde(default)]
    pub bucket: String,
    pub index: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub size: u64,
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub shards: Vec<ShardInfo>,
}

impl BucketFileInfo {
    pub fn is_legacy(&self) -> bool {
        self.version != Some(2) && self.shards.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShardInfo {
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub hash: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mirror {
    #[serde(default)]
    pub index: i64,
    #[serde(default)]
    pub hash: String,
    #[serde(default, deserialize_with = "de_u64_lenient")]
    pub size: u64,
    #[serde(default)]
    pub parity: bool,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StartUploadResponse {
    pub uploads: Vec<UploadSlot>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UploadSlot {
    pub uuid: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub urls: Option<Vec<String>>,
    #[serde(default, rename = "UploadId")]
    pub upload_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FinishUploadResponse {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_dto_lenient() {
        let f: FileDto = serde_json::from_str(
            r#"{"uuid":"u1","fileId":"f1","plainName":"report","type":"pdf","size":"1234",
                "folderUuid":"p","status":"EXISTS","updatedAt":"2024-05-01T10:00:00.000Z"}"#,
        )
        .unwrap();
        assert_eq!(f.size, 1234);
        assert_eq!(f.display_name(), "report.pdf");
        assert_eq!(f.status(), ItemStatus::Exists);
        assert!(f.updated_at.is_some());

        let g: FileDto = serde_json::from_str(r#"{"uuid":"u2","plainName":"Makefile","type":"","size":null,"status":"TRASHED"}"#).unwrap();
        assert_eq!(g.display_name(), "Makefile");
        assert_eq!(g.size, 0);
        assert_eq!(g.status(), ItemStatus::Trashed);
    }
}
