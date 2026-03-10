use axum::{
    Router, routing::{get, post},
    response::{Html, Redirect},
    extract::State,
    Form,
};
use serde::Deserialize;
use tower_sessions::Session;
use tracing::info;

use super::AppState;
use crate::db;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page))
        .route("/login", post(login_submit))
        .route("/logout", post(logout))
}

async fn login_page(session: Session) -> Html<String> {
    // If already logged in, redirect to home
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    if user_id.is_some() {
        return Html(r#"<meta http-equiv="refresh" content="0;url=/">"#.into());
    }
    Html(render_login(None))
}

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
}

async fn login_submit(
    session: Session,
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Html<String> {
    // Look up user by email
    let user = match db::find_user_by_email(&state.db, &form.email).await {
        Ok(Some(u)) => u,
        Ok(None) => return Html(render_login(Some("Invalid email or password"))),
        Err(e) => {
            tracing::error!("Database error: {e}");
            return Html(render_login(Some("Internal error")));
        }
    };

    // Verify bcrypt password
    // PHP bcrypt uses $2y$ prefix; the bcrypt crate handles this
    let valid = bcrypt::verify(&form.password, &user.password_hash).unwrap_or(false);
    if !valid {
        return Html(render_login(Some("Invalid email or password")));
    }

    // Set session
    if let Err(e) = session.insert("user_id", user.id).await {
        tracing::error!("Session error: {e}");
        return Html(render_login(Some("Session error")));
    }
    if let Err(e) = session.insert("user_name", &user.name).await {
        tracing::error!("Session error: {e}");
    }

    info!("User {} logged in", user.email);
    Html(r#"<meta http-equiv="refresh" content="0;url=/">"#.into())
}

async fn logout(session: Session) -> Redirect {
    let _ = session.delete().await;
    Redirect::to("/auth/login")
}

fn render_login(error: Option<&str>) -> String {
    let error_html = error.map(|e| format!(
        r#"<div class="card" style="border-color: var(--danger); margin-block-end: var(--space-4);">
            <p style="color: var(--danger); margin: 0;">{e}</p>
        </div>"#
    )).unwrap_or_default();

    format!(r#"<!DOCTYPE html>
<html lang="en" class="dark">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Login — Cosmix</title>
    <link rel="stylesheet" href="/static/base.css">
    <link rel="stylesheet" href="/static/app.css">
</head>
<body>
    <main>
        <div style="max-width: 400px; margin: 10vh auto;">
            <h1 style="text-align: center;">Cosmix</h1>
            {error_html}
            <div class="card">
                <form method="post" action="/auth/login">
                    <div class="form-group">
                        <label for="email">Email</label>
                        <input type="email" id="email" name="email" required autofocus>
                    </div>
                    <div class="form-group">
                        <label for="password">Password</label>
                        <input type="password" id="password" name="password" required>
                    </div>
                    <button type="submit" class="btn w-full">Sign in</button>
                </form>
            </div>
        </div>
    </main>
</body>
</html>"#)
}
