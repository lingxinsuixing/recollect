//! 一次性导出：把所有 system prompt 原样打出来（`cargo run -p rc-distill --example dump_prompts`）。
use rc_distill::pass_prompt::{
    fact_review_challenge_system, fact_review_retry_system, fact_review_system, fact_system,
    event_review_system, event_system, naming_review_system, rows_review_system,
};
use rc_distill::pass_prompt::FactFamily;
use rc_distill::prompt::{a1_system, audit_system, extract_system, judge_system};

fn show(name: &str, body: String) {
    println!("\n\n########## {name}  ({} chars) ##########\n{body}", body.len());
}

fn main() {
    show("extract_system (cycle 旧抽取)", extract_system());
    show("a1_system (naming 抽取)", a1_system());
    show("fact_system[decision]", fact_system(FactFamily::Decision));
    show("fact_system[promise]", fact_system(FactFamily::Promise));
    show("fact_system[plan]", fact_system(FactFamily::Plan));
    show("event_system", event_system());
    show("naming_review_system", naming_review_system());
    show("fact_review_system[decision]", fact_review_system(FactFamily::Decision));
    show("fact_review_system[promise]", fact_review_system(FactFamily::Promise));
    show("fact_review_system[plan]", fact_review_system(FactFamily::Plan));
    show("fact_review_challenge_system[decision]", fact_review_challenge_system(FactFamily::Decision));
    show("fact_review_retry_system", fact_review_retry_system());
    show("event_review_system", event_review_system());
    show("rows_review_system", rows_review_system());
    show("judge_system (dedupe)", judge_system());
    show("audit_system", audit_system());
}
