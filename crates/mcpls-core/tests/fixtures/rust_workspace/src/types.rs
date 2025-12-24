//! Custom types for definition and reference testing.

use crate::User;

/// A repository owned by a user.
#[derive(Debug, Clone)]
pub struct Repository {
    pub name: String,
    pub owner: User,
    pub stars: u32,
}

impl Repository {
    /// Creates a new repository with the given name and owner.
    pub fn new(name: String, owner: User) -> Self {
        Self {
            name,
            owner,
            stars: 0,
        }
    }

    /// Gets the owner of the repository.
    pub fn get_owner(&self) -> &User {
        &self.owner
    }
}
