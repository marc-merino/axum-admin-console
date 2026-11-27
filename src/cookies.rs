//! Reading request cookies and writing `Set-Cookie` values, by hand: the
//! console's cookies hold only alphanumeric and base64url text.

use axum::http::{HeaderMap, HeaderValue, header};

/// The value of cookie `name` in the request, if sent.
pub fn get(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().trim_matches('"').to_string())
}

pub struct SetCookie<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub path: &'a str,
    pub http_only: bool,
    pub secure: bool,
    /// `Some(0)` removes the cookie.
    pub max_age: Option<i64>,
}

impl SetCookie<'_> {
    /// The header value, attributes in the order the `cookie` crate writes
    /// them.
    pub fn header(&self) -> HeaderValue {
        let mut out = format!("{}={}", self.name, self.value);
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        out.push_str("; SameSite=Lax");
        if self.secure {
            out.push_str("; Secure");
        }
        out.push_str("; Path=");
        out.push_str(self.path);
        match self.max_age {
            Some(0) => out.push_str("; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(seconds) => out.push_str(&format!("; Max-Age={seconds}")),
            None => {}
        }
        HeaderValue::from_str(&out).unwrap_or_else(|_| HeaderValue::from_static(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_one_cookie_among_several() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "a=1; csrftoken=xyz".parse().unwrap());
        headers.append(header::COOKIE, "b=2".parse().unwrap());
        assert_eq!(get(&headers, "csrftoken").as_deref(), Some("xyz"));
        assert_eq!(get(&headers, "b").as_deref(), Some("2"));
        assert_eq!(get(&headers, "c"), None);
    }

    #[test]
    fn writes_attributes() {
        let cookie = SetCookie {
            name: "m",
            value: "v",
            path: "/admin/",
            http_only: true,
            secure: true,
            max_age: Some(0),
        };
        assert_eq!(
            cookie.header().to_str().unwrap(),
            "m=v; HttpOnly; SameSite=Lax; Secure; Path=/admin/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }
}
