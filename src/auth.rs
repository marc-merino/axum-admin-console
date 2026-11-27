//! What the host application tells the console about who is asking: the
//! [`Auth`] hook (required), optional username/password sign-in
//! ([`PasswordSignIn`]) and the password hashing behind [`Kind::Password`]
//! fields ([`Passwords`]).
//!
//! [`Kind::Password`]: crate::registry::Kind::Password

use axum::http::{HeaderMap, request::Parts};

use crate::{
    key::Key,
    registry::{BoxFuture, Model},
};

/// Who sent a request.
pub enum Identity {
    /// A signed-in, active staff member.
    Staff(StaffMember),
    /// Not signed in.
    Anonymous,
    /// Signed in, but not staff. `name` appears in Django's "You are
    /// authenticated as …, but are not authorized" note.
    NotStaff { name: String },
    /// The host could not tell (its database or configuration failed). The
    /// console answers with a server error.
    Unavailable(anyhow::Error),
}

/// A staff member as the host knows them.
pub struct StaffMember {
    /// A stable identifier as text (the user's primary key): what history
    /// entries are recorded under.
    pub id: String,
    /// The name the console greets them with.
    pub name: String,
    pub perms: Perms,
}

/// Answers who sent a request, from its headers (a session cookie, a
/// header set by a proxy). It runs once for every console request.
pub trait Auth: Send + Sync + 'static {
    fn identify<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Identity>;
}

/// A staff member's permissions with Django's meaning: codenames
/// `view_<model>`, `add_<model>`, `change_<model>` and `delete_<model>` of
/// an app label; change implies view; any permission in an app gives access
/// to the app (`has_module_permission`).
#[derive(Clone, Debug, Default)]
pub struct Perms {
    superuser: bool,
    granted: Vec<(String, String)>,
}

impl Perms {
    /// Every permission (a Django superuser).
    pub fn all() -> Self {
        Self {
            superuser: true,
            granted: Vec::new(),
        }
    }

    /// Exactly these `(app label, codename)` permissions.
    pub fn granted(granted: Vec<(String, String)>) -> Self {
        Self {
            superuser: false,
            granted,
        }
    }

    pub fn has(&self, app: &str, codename: &str) -> bool {
        self.superuser || self.granted.iter().any(|(a, c)| a == app && c == codename)
    }
    fn model(&self, model: &Model, action: &str) -> bool {
        self.has(model.app, &format!("{action}_{}", model.name))
    }
    /// Django: view or change permission.
    pub fn view(&self, model: &Model) -> bool {
        self.model(model, "view") || self.change(model)
    }
    pub fn add(&self, model: &Model) -> bool {
        model.can_add && self.model(model, "add")
    }
    pub fn change(&self, model: &Model) -> bool {
        model.can_change && self.model(model, "change")
    }
    pub fn delete(&self, model: &Model) -> bool {
        model.can_delete && self.model(model, "delete")
    }
    /// Any permission in the app (`has_module_permission`).
    pub fn module(&self, app: &str) -> bool {
        self.superuser || self.granted.iter().any(|(a, _)| a == app)
    }
}

/// What a sign-in attempt came to.
pub enum SignInOutcome {
    /// Signed in as staff. The headers (the host's `Set-Cookie` for its
    /// session) go on the redirect that follows.
    SignedIn(HeaderMap),
    /// Wrong credentials, or an account that is not staff: Django's "Please
    /// enter the correct username and password for a staff account".
    Refused,
    /// Too many attempts: answered with 429.
    Throttled,
    /// The host could not check (its database failed): a server error.
    Unavailable(anyhow::Error),
}

/// Username and password sign-in on the console's own form at
/// `/admin/login/`, and sign-out at `/admin/logout/`. The console checks
/// CSRF and the form, renders the pages and rotates its CSRF cookie; the
/// host checks the credentials and keeps the session.
pub trait PasswordSignIn: Send + Sync + 'static {
    /// Called with a non-empty username and password.
    fn sign_in<'a>(
        &'a self,
        parts: &'a Parts,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, SignInOutcome>;

    /// Ends the request's session; returns the headers (cookie removals)
    /// for the "Logged out" page.
    fn sign_out<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, anyhow::Result<HeaderMap>>;
}

/// The host's password hashing, for models with a [`Kind::Password`] field:
/// the add form's `password1`/`password2` pair (Django's
/// `UserCreationForm`) stores [`Passwords::hash`] of the new password.
///
/// [`Kind::Password`]: crate::registry::Kind::Password
pub trait Passwords: Send + Sync + 'static {
    /// The value stored for a new password.
    fn hash(&self, raw: String) -> BoxFuture<'static, anyhow::Result<String>>;

    /// What the change page shows for a stored hash. The default is Django's
    /// `ReadOnlyPasswordHashWidget` text for its `pbkdf2` hashes.
    fn summary(&self, stored: &str) -> String {
        crate::html::password_summary(stored)
    }

    /// A page where staff set a new password for this row, linked from its
    /// change page (a host page, see [`crate::Pages`]). `None`: no link.
    fn change_url(&self, model: &Model, key: &Key) -> Option<String> {
        let _ = (model, key);
        None
    }
}
