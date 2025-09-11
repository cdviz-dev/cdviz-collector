use crate::errors::{Result, miette};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub trait PathExt {
    fn extract_filename(&self) -> Result<Cow<'_, str>>;
}

impl PathExt for Path {
    fn extract_filename(&self) -> Result<Cow<'_, str>> {
        self.file_name()
            .ok_or(miette!("could not extract filename"))
            .map(|name| name.to_string_lossy())
    }
}

impl PathExt for PathBuf {
    fn extract_filename(&self) -> Result<Cow<'_, str>> {
        self.file_name()
            .ok_or(miette!("could not extract filename"))
            .map(|name| name.to_string_lossy())
    }
}
