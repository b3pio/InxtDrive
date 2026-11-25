use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::Method;
use serde_json::json;

use crate::client::{Client, Empty, Retry};
use crate::error::Result;
use crate::types::{CreateFileRequest, FileDto, FilePage, FolderDto, FolderPage, Limit, Paged, Usage};

pub const LIST_PAGE: u32 = 250;
pub const SYNC_PAGE: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Folder,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::File => "file",
            ItemKind::Folder => "folder",
        }
    }
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Splits a display name into the (plainName, type) pair Drive stores.
pub fn split_name(name: &str) -> (String, Option<String>) {
    match name.rfind('.') {
        Some(0) | None => (name.to_string(), None),
        Some(i) if i == name.len() - 1 => (name.to_string(), None),
        Some(i) => (name[..i].to_string(), Some(name[i + 1..].to_string())),
    }
}

impl Client {
    pub async fn list_folders_page(&self, parent: &str, cursor: Option<&str>) -> Result<Paged<FolderDto>> {
        let mut q = vec![("limit", LIST_PAGE.to_string()), ("order", "ASC".to_string())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        }
        let p: FolderPage = self
            .drive_call("list folders", Method::GET, &format!("/folders/v2/content/{}/folders", enc(parent)), &q, None, Retry::Idempotent)
            .await?;
        Ok(Paged { items: p.folders, next_cursor: p.next_cursor.filter(|c| !c.is_empty()) })
    }

    pub async fn list_files_page(&self, parent: &str, cursor: Option<&str>) -> Result<Paged<FileDto>> {
        let mut q = vec![("limit", LIST_PAGE.to_string()), ("order", "ASC".to_string())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        }
        let p: FilePage = self
            .drive_call("list files", Method::GET, &format!("/folders/v2/content/{}/files", enc(parent)), &q, None, Retry::Idempotent)
            .await?;
        Ok(Paged { items: p.files, next_cursor: p.next_cursor.filter(|c| !c.is_empty()) })
    }

    pub async fn list_children(&self, parent: &str) -> Result<(Vec<FolderDto>, Vec<FileDto>)> {
        let mut folders = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.list_folders_page(parent, cursor.as_deref()).await?;
            folders.extend(page.items);
            match page.next_cursor {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => break,
            }
        }
        let mut files = Vec::new();
        cursor = None;
        loop {
            let page = self.list_files_page(parent, cursor.as_deref()).await?;
            files.extend(page.items);
            match page.next_cursor {
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => break,
            }
        }
        Ok((folders, files))
    }

    /// Account-wide delta of files changed after `since` (all statuses).
    /// Continue with the returned cursor until it is `None`.
    pub async fn files_sync_page(&self, since: DateTime<Utc>, cursor: Option<&str>) -> Result<Paged<FileDto>> {
        let mut q = vec![("limit", SYNC_PAGE.to_string())];
        match cursor {
            Some(c) => q.push(("cursor", c.to_string())),
            None => q.push(("updatedAt", iso(since))),
        }
        let p: FilePage = self.drive_call("sync files", Method::GET, "/files/sync", &q, None, Retry::Idempotent).await?;
        Ok(Paged { items: p.files, next_cursor: p.next_cursor.filter(|c| !c.is_empty()) })
    }

    pub async fn folders_sync_page(&self, since: DateTime<Utc>, cursor: Option<&str>) -> Result<Paged<FolderDto>> {
        let mut q = vec![("limit", SYNC_PAGE.to_string())];
        match cursor {
            Some(c) => q.push(("cursor", c.to_string())),
            None => q.push(("updatedAt", iso(since))),
        }
        let p: FolderPage = self.drive_call("sync folders", Method::GET, "/folders/sync", &q, None, Retry::Idempotent).await?;
        Ok(Paged { items: p.folders, next_cursor: p.next_cursor.filter(|c| !c.is_empty()) })
    }

    pub async fn get_folder(&self, uuid: &str) -> Result<FolderDto> {
        self.drive_call("get folder", Method::GET, &format!("/folders/{}/meta", enc(uuid)), &[], None, Retry::Idempotent)
            .await
    }

    pub async fn get_file(&self, uuid: &str) -> Result<FileDto> {
        self.drive_call("get file", Method::GET, &format!("/files/{}/meta", enc(uuid)), &[], None, Retry::Idempotent)
            .await
    }

    pub async fn create_folder(&self, parent: &str, name: &str, ctime: DateTime<Utc>, mtime: DateTime<Utc>) -> Result<FolderDto> {
        let body = json!({
            "plainName": name,
            "parentFolderUuid": parent,
            "creationTime": iso(ctime),
            "modificationTime": iso(mtime),
        });
        self.drive_call("create folder", Method::POST, "/folders", &[], Some(body), Retry::Once).await
    }

    pub async fn rename_folder(&self, uuid: &str, name: &str) -> Result<()> {
        let _: Empty = self
            .drive_call("rename folder", Method::PUT, &format!("/folders/{}/meta", enc(uuid)), &[], Some(json!({ "plainName": name })), Retry::Idempotent)
            .await
            .or_else(empty_ok)?;
        Ok(())
    }

    pub async fn move_folder(&self, uuid: &str, destination: &str, new_name: Option<&str>) -> Result<()> {
        let mut body = json!({ "destinationFolder": destination });
        if let Some(n) = new_name {
            body["name"] = json!(n);
        }
        let _: Empty = self
            .drive_call("move folder", Method::PATCH, &format!("/folders/{}", enc(uuid)), &[], Some(body), Retry::Idempotent)
            .await
            .or_else(empty_ok)?;
        Ok(())
    }

    pub async fn create_file(&self, req: &CreateFileRequest) -> Result<FileDto> {
        let body = serde_json::to_value(req).expect("serializable");
        self.drive_call("create file", Method::POST, "/files", &[], Some(body), Retry::Once).await
    }

    /// Points an existing file at new content, keeping its uuid, name and history.
    pub async fn replace_file(&self, uuid: &str, file_id: Option<&str>, size: u64, mtime: DateTime<Utc>) -> Result<FileDto> {
        let mut body = json!({ "size": size, "modificationTime": iso(mtime) });
        if let Some(id) = file_id {
            body["fileId"] = json!(id);
        }
        self.drive_call("replace file", Method::PUT, &format!("/files/{}", enc(uuid)), &[], Some(body), Retry::Idempotent).await
    }

    pub async fn rename_file(&self, uuid: &str, name: &str) -> Result<()> {
        let (plain, ty) = split_name(name);
        let body = json!({ "plainName": plain, "type": ty.unwrap_or_default() });
        let _: Empty = self
            .drive_call("rename file", Method::PUT, &format!("/files/{}/meta", enc(uuid)), &[], Some(body), Retry::Idempotent)
            .await
            .or_else(empty_ok)?;
        Ok(())
    }

    pub async fn move_file(&self, uuid: &str, destination: &str, new_name: Option<&str>) -> Result<()> {
        let mut body = json!({ "destinationFolder": destination });
        if let Some(n) = new_name {
            let (plain, ty) = split_name(n);
            body["name"] = json!(plain);
            body["type"] = json!(ty.unwrap_or_default());
        }
        let _: Empty = self
            .drive_call("move file", Method::PATCH, &format!("/files/{}", enc(uuid)), &[], Some(body), Retry::Idempotent)
            .await
            .or_else(empty_ok)?;
        Ok(())
    }

    /// Moves items to the Internxt trash (recoverable from the web app).
    pub async fn trash(&self, items: &[(ItemKind, String)]) -> Result<()> {
        for chunk in items.chunks(50) {
            let list: Vec<_> = chunk.iter().map(|(k, u)| json!({ "type": k.as_str(), "uuid": u })).collect();
            self.drive_call_empty("trash", Method::POST, "/storage/trash/add", Some(json!({ "items": list })), Retry::Idempotent)
                .await?;
        }
        Ok(())
    }

    pub async fn usage_bytes(&self) -> Result<u64> {
        let u: Usage = self.drive_call("usage", Method::GET, "/users/usage", &[], None, Retry::Idempotent).await?;
        Ok(u.drive)
    }

    pub async fn limit_bytes(&self) -> Result<u64> {
        let l: Limit = self.drive_call("limit", Method::GET, "/users/limit", &[], None, Retry::Idempotent).await?;
        Ok(l.max_space_bytes)
    }
}

/// Some mutation endpoints answer with an empty or non-JSON body.
fn empty_ok(e: crate::ApiError) -> Result<Empty> {
    match e {
        crate::ApiError::Decode(..) => Ok(Empty {}),
        other => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::split_name;

    #[test]
    fn splits_like_drive() {
        assert_eq!(split_name("report.final.pdf"), ("report.final".into(), Some("pdf".into())));
        assert_eq!(split_name("Makefile"), ("Makefile".into(), None));
        assert_eq!(split_name(".gitignore"), (".gitignore".into(), None));
        assert_eq!(split_name("trailing."), ("trailing.".into(), None));
    }
}
