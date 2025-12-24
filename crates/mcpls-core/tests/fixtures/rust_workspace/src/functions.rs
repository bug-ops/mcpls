//! Functions for reference finding tests.

use crate::types::Repository;
use crate::User;

/// Creates a repository with a default test user.
pub fn create_repo(name: &str) -> Repository {
    let owner = User::new(
        1,
        "Test User".to_string(),
        "test@example.com".to_string(),
    );
    Repository::new(name.to_string(), owner)
}

/// Gets the name of a repository.
pub fn get_repo_name(repo: &Repository) -> &str {
    &repo.name
}

/// Gets the owner's name from a repository.
pub fn get_owner_name(repo: &Repository) -> &str {
    &repo.get_owner().name
}

/// Increments the star count for a repository.
pub fn add_star(repo: &mut Repository) {
    repo.stars += 1;
}
