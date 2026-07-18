//! No-reply sentinels and WeChat text sanitization for outbound replies.

/// Does this agent reply text mean "no reply should be sent to the user"?
///
/// Agents/flows signal an intentional no-reply by emitting a sentinel token as
/// the *entire* reply (`[no reply needed]`, `[no_reply_needed]`, `[无需回复]`,
/// `NO_REPLY`, …). When the WeChat OA 客服消息 48h window is closed — which is
/// the common case for event-triggered turns (card clicks, page views) that
/// don't carry a real user message — shipping that sentinel via the customer-
/// send API yields error 45015 and spams the logs (~dozens/day). Suppressing
/// it here also stops the literal marker from ever reaching a user on any
/// channel. Whole-text match only (after trimming brackets), so a real reply
/// that merely contains the phrase is untouched.
///
/// Called from BOTH delivery sinks: the interactive `send_response` path and
/// the cron `cron_deliver_response` path (kernel/daemon.rs), so a scheduled
/// turn that resolves to "no reply" is suppressed everywhere — not just inline.
pub fn is_no_reply_sentinel(text: &str) -> bool {
    let t = text.trim();
    let inner = t
        .trim_start_matches(['[', '【'])
        .trim_end_matches([']', '】'])
        .trim()
        .to_lowercase();
    // Normalize underscores to spaces so `[no_reply_needed]` == `[no reply needed]`.
    let inner = inner.replace('_', " ");
    matches!(
        inner.as_str(),
        "no reply needed"
            | "no reply"
            | "noreply"
            | "no reply required"
            | "无需回复"
            | "无需答复"
    )
}

/// Strip characters that WeChat iLink/OA cannot render (shows as ???).
/// Keeps: CJK, ASCII, common punctuation, newlines. Removes: emoji,
/// variation selectors, miscellaneous symbols, ornamental characters.
pub fn sanitize_wechat_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            // Basic ASCII (printable + newline/tab)
            if c.is_ascii() {
                return !c.is_control() || *c == '\n' || *c == '\t';
            }
            // CJK Unified Ideographs
            if matches!(c, '\u{4E00}'..='\u{9FFF}') {
                return true;
            }
            // CJK Extension A & B
            if matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{20000}'..='\u{2A6DF}') {
                return true;
            }
            // Fullwidth forms (fullwidth ASCII, punctuation)
            if matches!(c, '\u{FF01}'..='\u{FF5E}' | '\u{3000}'..='\u{303F}') {
                return true;
            }
            // CJK compatibility, Kangxi radical, Bopomofo, Hiragana, Katakana
            if matches!(c, '\u{F900}'..='\u{FAFF}' | '\u{2F00}'..='\u{2FDF}'
                        | '\u{3100}'..='\u{318F}' | '\u{3040}'..='\u{309F}'
                        | '\u{30A0}'..='\u{30FF}') {
                return true;
            }
            // Common punctuation (general + CJK-specific)
            if matches!(c, '—' | '–' | '…' | '·' | '×' | '÷' | '°' | '℃'
                        | '←' | '→' | '↑' | '↓' | '■' | '□' | '▪' | '▶'
                        | '《' | '》' | '〈' | '〉' | '【' | '】' | '〖' | '〗'
                        | '「' | '」' | '『' | '』' | '﹏' | '￥' | '＄' | '€') {
                return true;
            }
            // Latin-1 Supplement (accented chars, copyright, registered, etc.)
            if matches!(c, '\u{00A0}'..='\u{00FF}') {
                return true;
            }
            // Common letter/number ranges (Latin Extended, Greek, Cyrillic)
            if c.is_alphanumeric() {
                return true;
            }
            // General punctuation (quotes, dashes, brackets)
            if matches!(c, '\u{2010}'..='\u{205F}') {
                return true;
            }
            false
        })
        .collect::<String>()
}
