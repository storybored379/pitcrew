//! Hypothesis prototype for completed-download isolation, not an arr API emulator.
//!
//! Category names are synthetic. Real adapters must prove equivalent capabilities
//! against each supported upstream version before selecting the held route.
use pitcrew_domain::FileIdentity;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldingSettings {
    pub watched_categories: Vec<String>,
    /// Assigned at submission, before the download can finish.
    pub submission_category: String,
    pub can_choose_category_before_submission: bool,
    pub can_manual_import_exact_file: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportRoute {
    Held,
    VerifyAfterImport,
}

impl HoldingSettings {
    pub fn route(&self) -> ImportRoute {
        if self.can_choose_category_before_submission
            && self.can_manual_import_exact_file
            && !self.submission_category.is_empty()
            && !self.watched_categories.contains(&self.submission_category)
        {
            ImportRoute::Held
        } else {
            ImportRoute::VerifyAfterImport
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeDownload {
    pub id: u64,
    pub file: FileIdentity,
    pub category: String,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportCall {
    AutoImport(FileIdentity),
    ManualImport(FileIdentity),
}

#[derive(Clone, Debug)]
pub struct HoldingArr {
    pub settings: HoldingSettings,
    pub library_file: FileIdentity,
    pub download: Option<FakeDownload>,
    pub calls: Vec<ImportCall>,
}

impl HoldingArr {
    /// Simulates normal completed-download handling, including a broken upgrade.
    /// No implicit decode or quality guard lives inside the fake upstream.
    pub fn completed_download_handling(&mut self) {
        if let Some(download) = &self.download
            && download.complete
            && self
                .settings
                .watched_categories
                .contains(&download.category)
        {
            self.library_file = download.file;
            self.calls.push(ImportCall::AutoImport(download.file));
            self.download = None;
        }
    }

    pub fn manual_import(
        &mut self,
        download_id: u64,
        file: FileIdentity,
    ) -> Result<(), &'static str> {
        if !self.settings.can_manual_import_exact_file {
            return Err("manual import unavailable");
        }
        let download = self.download.as_ref().ok_or("download absent")?;
        if !download.complete || download.id != download_id || download.file != file {
            return Err("exact completed source absent or changed");
        }
        self.library_file = file;
        self.download = None;
        self.calls.push(ImportCall::ManualImport(file));
        Ok(())
    }
}
