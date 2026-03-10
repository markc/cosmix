use anyhow::Result;
use sea_orm::{ConnectionTrait, DatabaseConnection, Database};
use serde::{Deserialize, Serialize};

pub async fn connect(url: &str) -> Result<DatabaseConnection> {
    let db = Database::connect(url).await?;
    Ok(db)
}

/// Minimal user struct — only the fields we need for auth and display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub password_hash: String,
}

/// Find a user by email (raw query since we don't have generated entities yet).
pub async fn find_user_by_email(db: &DatabaseConnection, email: &str) -> Result<Option<User>> {
    use sea_orm::{Statement, DbBackend, FromQueryResult};

    #[derive(Debug, FromQueryResult)]
    struct UserRow {
        id: i64,
        name: String,
        email: String,
        password: String,
    }

    let row = UserRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id, name, email, password FROM users WHERE email = $1 LIMIT 1",
        [email.into()],
    ))
    .one(db)
    .await?;

    Ok(row.map(|r| User {
        id: r.id,
        name: r.name,
        email: r.email,
        password_hash: r.password,
    }))
}

/// Find a user by ID.
pub async fn find_user_by_id(db: &DatabaseConnection, id: i64) -> Result<Option<User>> {
    use sea_orm::{Statement, DbBackend, FromQueryResult};

    #[derive(Debug, FromQueryResult)]
    struct UserRow {
        id: i64,
        name: String,
        email: String,
        password: String,
    }

    let row = UserRow::find_by_statement(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id, name, email, password FROM users WHERE id = $1 LIMIT 1",
        [id.into()],
    ))
    .one(db)
    .await?;

    Ok(row.map(|r| User {
        id: r.id,
        name: r.name,
        email: r.email,
        password_hash: r.password,
    }))
}

/// Update a user's name.
pub async fn update_user_name(db: &DatabaseConnection, id: i64, name: &str) -> Result<()> {
    use sea_orm::{Statement, DbBackend};

    db.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE users SET name = $1 WHERE id = $2",
        [name.into(), id.into()],
    )).await?;

    Ok(())
}

/// Update a user's password hash.
pub async fn update_user_password(db: &DatabaseConnection, id: i64, password_hash: &str) -> Result<()> {
    use sea_orm::{Statement, DbBackend};

    db.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE users SET password = $1 WHERE id = $2",
        [password_hash.into(), id.into()],
    )).await?;

    Ok(())
}
