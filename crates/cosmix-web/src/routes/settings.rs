use axum::{
    Router, routing::{get, post},
    response::Html,
    extract::State,
    Form,
};
use serde::Deserialize;
use tower_sessions::Session;

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_page))
        .route("/settings", post(save_settings))
        .route("/settings/password", post(change_password))
}

/// Render settings page with current user data.
async fn settings_page(
    session: Session,
    State(state): State<AppState>,
) -> Html<String> {
    let user_id: i64 = session.get("user_id").await.unwrap_or(None).unwrap_or(0);
    let user = crate::db::find_user_by_id(&state.db, user_id).await.ok().flatten();

    let (name, email) = match user {
        Some(u) => (u.name, u.email),
        None => ("Unknown".into(), "unknown@example.com".into()),
    };

    Html(format!(r#"<div class="page-settings">
    <h2>Settings</h2>
    <form hx-post="/api/settings" hx-swap="outerHTML">
        <fieldset>
            <legend>Account</legend>
            <div class="form-group">
                <label for="email">Email</label>
                <input type="email" id="email" name="email" value="{email}" disabled>
            </div>
            <div class="form-group">
                <label for="name">Name</label>
                <input type="text" id="name" name="name" value="{name}">
            </div>
        </fieldset>
        <button type="submit" class="btn">Save</button>
    </form>

    <form hx-post="/api/settings/password" hx-swap="outerHTML" style="margin-top: var(--space-6);">
        <fieldset>
            <legend>Change Password</legend>
            <div class="form-group">
                <label for="current_password">Current Password</label>
                <input type="password" id="current_password" name="current_password" required>
            </div>
            <div class="form-group">
                <label for="new_password">New Password</label>
                <input type="password" id="new_password" name="new_password" required minlength="8">
            </div>
        </fieldset>
        <button type="submit" class="btn">Change Password</button>
    </form>
</div>"#))
}

#[derive(Deserialize)]
struct SettingsForm {
    name: String,
}

async fn save_settings(
    session: Session,
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Html<String> {
    let user_id: i64 = session.get("user_id").await.unwrap_or(None).unwrap_or(0);

    match crate::db::update_user_name(&state.db, user_id, &form.name).await {
        Ok(()) => {
            let _ = session.insert("user_name", &form.name).await;
            Html(r#"<div class="toast toast-success">Settings saved</div>"#.into())
        }
        Err(e) => Html(format!(r#"<div class="toast toast-error">Error: {e}</div>"#)),
    }
}

#[derive(Deserialize)]
struct PasswordForm {
    current_password: String,
    new_password: String,
}

async fn change_password(
    session: Session,
    State(state): State<AppState>,
    Form(form): Form<PasswordForm>,
) -> Html<String> {
    let user_id: i64 = session.get("user_id").await.unwrap_or(None).unwrap_or(0);

    let user = match crate::db::find_user_by_id(&state.db, user_id).await {
        Ok(Some(u)) => u,
        _ => return Html(r#"<div class="toast toast-error">User not found</div>"#.into()),
    };

    let valid = bcrypt::verify(&form.current_password, &user.password_hash).unwrap_or(false);
    if !valid {
        return Html(r#"<div class="toast toast-error">Current password is incorrect</div>"#.into());
    }

    let new_hash = match bcrypt::hash(&form.new_password, 12) {
        Ok(h) => h,
        Err(e) => return Html(format!(r#"<div class="toast toast-error">Error: {e}</div>"#)),
    };

    match crate::db::update_user_password(&state.db, user_id, &new_hash).await {
        Ok(()) => Html(r#"<div class="toast toast-success">Password changed</div>"#.into()),
        Err(e) => Html(format!(r#"<div class="toast toast-error">Error: {e}</div>"#)),
    }
}
