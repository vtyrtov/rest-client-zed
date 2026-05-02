use crate::executor::Response;

pub fn format_response(response: &Response) -> String {
    let mut out = String::new();

    // Status line
    out.push_str(&format!(
        "HTTP {} {}\n",
        response.status, response.status_text
    ));
    out.push_str(&format!("Time: {}ms\n", response.elapsed_ms));
    out.push('\n');

    // Headers
    for (name, value) in &response.headers {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out.push('\n');

    // Body
    let body = format_response_body(&response.body, &response.headers);
    out.push_str(&body);

    out
}

pub fn format_response_diagnostics(response: &Response) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "HTTP {} {}\n",
        response.status, response.status_text
    ));
    out.push_str(&format!("Time: {}ms\n", response.elapsed_ms));
    out.push('\n');
    for (name, value) in &response.headers {
        out.push_str(&format!("{name}: {value}\n"));
    }
    out
}

pub fn format_response_body(body: &str, headers: &[(String, String)]) -> String {
    let content_type = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    if content_type.contains("json") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                return pretty;
            }
        }
    }

    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_json_response() {
        let response = Response {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: r#"{"key":"value","num":42}"#.to_string(),
            elapsed_ms: 150,
        };

        let formatted = format_response(&response);
        assert!(formatted.contains("HTTP 200 OK"));
        assert!(formatted.contains("Time: 150ms"));
        assert!(formatted.contains("\"key\": \"value\""));
    }

    #[test]
    fn test_format_plain_text() {
        let response = Response {
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: "Hello, World!".to_string(),
            elapsed_ms: 50,
        };

        let formatted = format_response(&response);
        assert!(formatted.contains("Hello, World!"));
    }
}
