// Proxy debug logging types and format functions
//
// Contains DebugEvent enum and format_*_log helper functions
// used by SipProxy for structured debug log output.

use std::net::SocketAddr;

pub(crate) enum DebugEvent {
    RecvReq,
    FwdReq,
    RecvRes,
    FwdRes,
    Error,
}

impl DebugEvent {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            DebugEvent::RecvReq => "RECV_REQ",
            DebugEvent::FwdReq => "FWD_REQ",
            DebugEvent::RecvRes => "RECV_RES",
            DebugEvent::FwdRes => "FWD_RES",
            DebugEvent::Error => "ERROR",
        }
    }
}

/// リクエスト受信ログのフォーマット
pub(crate) fn format_recv_req_log(method: &str, request_uri: &str, from: SocketAddr, call_id: &str) -> String {
    format!(
        "[DEBUG] {} method={} uri={} from={} call-id={}",
        DebugEvent::RecvReq.label(),
        method,
        request_uri,
        from,
        call_id
    )
}

/// リクエスト転送ログのフォーマット
pub(crate) fn format_fwd_req_log(method: &str, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} method={} call-id={} dest={}",
        DebugEvent::FwdReq.label(),
        method,
        call_id,
        dest
    )
}

/// レスポンス受信ログのフォーマット
pub(crate) fn format_recv_res_log(status_code: u16, reason: &str, from: SocketAddr, call_id: &str) -> String {
    format!(
        "[DEBUG] {} status={} reason={} from={} call-id={}",
        DebugEvent::RecvRes.label(),
        status_code,
        reason,
        from,
        call_id
    )
}

/// レスポンス転送ログのフォーマット
pub(crate) fn format_fwd_res_log(status_code: u16, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} status={} call-id={} dest={}",
        DebugEvent::FwdRes.label(),
        status_code,
        call_id,
        dest
    )
}

/// エラーログのフォーマット（リクエスト転送エラー）
pub(crate) fn format_req_error_log(error: &str, method: &str, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} event={} error=\"{}\" method={} call-id={} dest={}",
        DebugEvent::Error.label(),
        DebugEvent::FwdReq.label(),
        error,
        method,
        call_id,
        dest
    )
}

/// エラーログのフォーマット（レスポンス転送エラー）
pub(crate) fn format_res_error_log(error: &str, status_code: u16, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} event={} error=\"{}\" status={} call-id={} dest={}",
        DebugEvent::Error.label(),
        DebugEvent::FwdRes.label(),
        error,
        status_code,
        call_id,
        dest
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_event_labels() {
        assert_eq!(DebugEvent::RecvReq.label(), "RECV_REQ");
        assert_eq!(DebugEvent::FwdReq.label(), "FWD_REQ");
        assert_eq!(DebugEvent::RecvRes.label(), "RECV_RES");
        assert_eq!(DebugEvent::FwdRes.label(), "FWD_RES");
        assert_eq!(DebugEvent::Error.label(), "ERROR");
    }

    #[test]
    fn test_format_recv_req_log() {
        let from: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_recv_req_log("INVITE", "sip:bob@example.com", from, "abc123@host");
        assert_eq!(
            result,
            "[DEBUG] RECV_REQ method=INVITE uri=sip:bob@example.com from=192.168.1.1:5060 call-id=abc123@host"
        );
    }

    #[test]
    fn test_format_fwd_req_log() {
        let dest: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_fwd_req_log("INVITE", "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] FWD_REQ method=INVITE call-id=abc123@host dest=10.0.0.1:5080"
        );
    }

    #[test]
    fn test_format_recv_res_log() {
        let from: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_recv_res_log(200, "OK", from, "abc123@host");
        assert_eq!(
            result,
            "[DEBUG] RECV_RES status=200 reason=OK from=10.0.0.1:5080 call-id=abc123@host"
        );
    }

    #[test]
    fn test_format_fwd_res_log() {
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_fwd_res_log(200, "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] FWD_RES status=200 call-id=abc123@host dest=192.168.1.1:5060"
        );
    }

    #[test]
    fn test_format_req_error_log() {
        let dest: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_req_error_log("connection refused", "INVITE", "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] ERROR event=FWD_REQ error=\"connection refused\" method=INVITE call-id=abc123@host dest=10.0.0.1:5080"
        );
    }

    #[test]
    fn test_format_res_error_log() {
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_res_error_log("connection refused", 200, "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] ERROR event=FWD_RES error=\"connection refused\" status=200 call-id=abc123@host dest=192.168.1.1:5060"
        );
    }
}
