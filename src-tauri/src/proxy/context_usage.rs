//! Distribution of the *outbound* context, without retaining content or guessing
//! a model's tokenizer. Counts payload bytes, not billable or visual tokens.
use serde::Serialize;
use serde_json::Value;
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Footprint {
    pub system: u64,
    pub tools: u64,
    pub chat: u64,
    pub results: u64,
    pub media: u64,
    pub partial: bool,
}
#[derive(Copy, Clone)]
enum Part {
    System,
    Tools,
    Chat,
    Results,
    Media,
}
struct Counter {
    out: Footprint,
    left: usize,
}
impl Counter {
    fn add(&mut self, p: Part, n: usize) {
        let slot = match p {
            Part::System => &mut self.out.system,
            Part::Tools => &mut self.out.tools,
            Part::Chat => &mut self.out.chat,
            Part::Results => &mut self.out.results,
            Part::Media => &mut self.out.media,
        };
        *slot = slot.saturating_add(n as u64);
    }
    fn visit(&mut self, v: &Value, part: Part, depth: usize) {
        if self.left == 0 || depth > 40 {
            self.out.partial = true;
            return;
        }
        self.left -= 1;
        match v {
            Value::String(s) => self.add(part, s.len()),
            Value::Array(xs) => {
                for x in xs {
                    if self.left == 0 {
                        self.out.partial = true;
                        break;
                    }
                    self.visit(x, part, depth + 1);
                }
            }
            Value::Object(o) => {
                let selected = match part {
                    Part::Tools | Part::Media => part,
                    _ => match o.get("role").and_then(Value::as_str) {
                        Some("developer" | "system") => Part::System,
                        Some("tool" | "function") => Part::Results,
                        _ => match o.get("type").and_then(Value::as_str) {
                            Some(
                                "tool_result" | "function_call_output" | "custom_tool_call_output",
                            ) => Part::Results,
                            Some(
                                "image" | "image_url" | "input_image" | "input_audio"
                                | "input_file" | "document",
                            ) => Part::Media,
                            _ => part,
                        },
                    },
                };
                for (k, x) in o {
                    if self.left == 0 {
                        self.out.partial = true;
                        break;
                    }
                    if matches!(selected, Part::Tools) {
                        self.add(selected, k.len());
                    } else if matches!(
                        k.as_str(),
                        "role" | "type" | "id" | "call_id" | "signature" | "encrypted_content"
                    ) {
                        continue;
                    }
                    let p = match k.as_str() {
                        "inlineData" | "inline_data" | "fileData" | "file_data" | "image_url" => {
                            Part::Media
                        }
                        "functionResponse" | "function_response" => Part::Results,
                        _ => selected,
                    };
                    self.visit(x, p, depth + 1);
                }
            }
            Value::Bool(_) | Value::Number(_) => self.add(part, 1),
            Value::Null => {}
        }
    }
}
pub(super) fn measure(body: &Value) -> Footprint {
    let mut c = Counter {
        out: Default::default(),
        left: 16_384,
    };
    for (key, part) in [
        ("system", Part::System),
        ("instructions", Part::System),
        ("systemInstruction", Part::System),
        ("system_instruction", Part::System),
        ("tools", Part::Tools),
        ("functions", Part::Tools),
        ("messages", Part::Chat),
        ("input", Part::Chat),
        ("contents", Part::Chat),
        ("prompt", Part::Chat),
    ] {
        if let Some(v) = body.get(key) {
            c.visit(v, part, 0);
        }
    }
    if body.get("previous_response_id").is_some() {
        c.out.partial = true;
    }
    c.out
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn groups_actual_payload_without_retaining_text() {
        let f = measure(
            &json!({"instructions":"秘密","tools":[{"name":"a"}],"input":[{"role":"user","content":"hello"},{"type":"function_call_output","output":"result"},{"type":"input_image","image_url":"base64"}],"previous_response_id":"prior"}),
        );
        assert_eq!((f.system, f.chat, f.results, f.media), (6, 5, 6, 6));
        assert!(f.tools > 0 && f.partial);
        assert!(!serde_json::to_string(&f).unwrap().contains("秘密"));
    }
}
