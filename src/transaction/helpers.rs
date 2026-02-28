use crate::sip::message::{Headers, Method, SipRequest, SipResponse};

pub fn extract_branch(via: &str) -> Option<String> {
    for part in via.split(';') {
        let trimmed = part.trim();
        if let Some(value) = trimmed.strip_prefix("branch=") {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// CSeq headerからメソッドを解析する
/// 形式: "1 INVITE" → Method::Invite
pub fn parse_method_from_cseq(cseq: &str) -> Option<Method> {
    let method_str = cseq.split_whitespace().nth(1)?;
    Some(match method_str {
        "REGISTER" => Method::Register,
        "INVITE" => Method::Invite,
        "ACK" => Method::Ack,
        "BYE" => Method::Bye,
        "OPTIONS" => Method::Options,
        "UPDATE" => Method::Update,
        other => Method::Other(other.to_string()),
    })
}

pub fn generate_ack(original_request: &SipRequest, response: &SipResponse) -> SipRequest {
    let mut headers = Headers::new();

    // Via: 元のINVITEの最初のViaのみ
    if let Some(via) = original_request.headers.get("Via") {
        headers.add("Via", via.to_string());
    }

    // From: 元のINVITEと同じ
    if let Some(from) = original_request.headers.get("From") {
        headers.add("From", from.to_string());
    }

    // To: レスポンスから（tagを含む）
    if let Some(to) = response.headers.get("To") {
        headers.add("To", to.to_string());
    } else if let Some(to) = original_request.headers.get("To") {
        headers.add("To", to.to_string());
    }

    // Call-ID: 元のINVITEと同じ
    if let Some(call_id) = original_request.headers.get("Call-ID") {
        headers.add("Call-ID", call_id.to_string());
    }

    // CSeq: 同じシーケンス番号、メソッド = ACK
    if let Some(cseq) = original_request.headers.get("CSeq") {
        if let Some(seq_num) = cseq.split_whitespace().next() {
            headers.add("CSeq", format!("{} ACK", seq_num));
        }
    }

    // Content-Length: 0
    headers.add("Content-Length", "0".to_string());

    SipRequest {
        method: Method::Ack,
        request_uri: original_request.request_uri.clone(),
        version: "SIP/2.0".to_string(),
        headers,
        body: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === extract_branch tests ===

    #[test]
    fn extract_branch_from_standard_via() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK776asdhds";
        assert_eq!(extract_branch(via), Some("z9hG4bK776asdhds".to_string()));
    }

    #[test]
    fn extract_branch_with_multiple_params() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060;rport;branch=z9hG4bK001;received=10.0.0.1";
        assert_eq!(extract_branch(via), Some("z9hG4bK001".to_string()));
    }

    #[test]
    fn extract_branch_missing() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060";
        assert_eq!(extract_branch(via), None);
    }

    // === parse_method_from_cseq tests ===

    #[test]
    fn parse_cseq_invite() {
        assert_eq!(parse_method_from_cseq("1 INVITE"), Some(Method::Invite));
    }

    #[test]
    fn parse_cseq_register() {
        assert_eq!(parse_method_from_cseq("1 REGISTER"), Some(Method::Register));
    }

    #[test]
    fn parse_cseq_bye() {
        assert_eq!(parse_method_from_cseq("2 BYE"), Some(Method::Bye));
    }

    #[test]
    fn parse_cseq_other_method() {
        assert_eq!(parse_method_from_cseq("1 SUBSCRIBE"), Some(Method::Other("SUBSCRIBE".to_string())));
    }

    #[test]
    fn parse_cseq_empty() {
        assert_eq!(parse_method_from_cseq(""), None);
    }

    #[test]
    fn parse_cseq_number_only() {
        assert_eq!(parse_method_from_cseq("1"), None);
    }
}
