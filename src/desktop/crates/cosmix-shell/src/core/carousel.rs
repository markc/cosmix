//! Stable page selection for one edge panel carousel.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

/// Ordered stable page IDs and the active selection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Carousel {
    page_ids: Arc<[String]>,
    active: usize,
}

impl Carousel {
    pub fn new(
        page_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, CarouselError> {
        let page_ids: Vec<String> = page_ids.into_iter().map(Into::into).collect();
        let mut seen = HashSet::with_capacity(page_ids.len());
        for id in &page_ids {
            if id.trim().is_empty() {
                return Err(CarouselError::EmptyId);
            }
            if !seen.insert(id.clone()) {
                return Err(CarouselError::DuplicateId(id.clone()));
            }
        }
        Ok(Self {
            page_ids: page_ids.into(),
            active: 0,
        })
    }

    pub fn empty() -> Self {
        Self {
            page_ids: Arc::from([]),
            active: 0,
        }
    }

    pub fn page_ids(&self) -> &[String] {
        &self.page_ids
    }

    /// Clone the shared immutable page schema without cloning its strings.
    pub fn shared_page_ids(&self) -> Arc<[String]> {
        Arc::clone(&self.page_ids)
    }

    pub fn active_index(&self) -> Option<usize> {
        (!self.page_ids.is_empty()).then_some(self.active)
    }

    pub fn active_id(&self) -> Option<&str> {
        self.page_ids.get(self.active).map(String::as_str)
    }

    pub fn next_page(&mut self) -> Option<&str> {
        if !self.page_ids.is_empty() {
            self.active = (self.active + 1) % self.page_ids.len();
        }
        self.active_id()
    }

    pub fn previous_page(&mut self) -> Option<&str> {
        if !self.page_ids.is_empty() {
            self.active = (self.active + self.page_ids.len() - 1) % self.page_ids.len();
        }
        self.active_id()
    }

    pub fn select_index(&mut self, index: usize) -> bool {
        if index >= self.page_ids.len() {
            return false;
        }
        self.active = index;
        true
    }

    pub fn select_id(&mut self, id: &str) -> bool {
        let Some(index) = self.page_ids.iter().position(|candidate| candidate == id) else {
            return false;
        };
        self.active = index;
        true
    }
}

/// Invalid stable page IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CarouselError {
    EmptyId,
    DuplicateId(String),
}

impl Display for CarouselError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("carousel page ID must not be empty"),
            Self::DuplicateId(id) => write!(formatter, "duplicate carousel page ID '{id}'"),
        }
    }
}

impl Error for CarouselError {}
