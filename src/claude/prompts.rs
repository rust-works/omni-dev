//! Prompt templates and engineering for Claude API

/// System prompt for commit message improvement
pub const SYSTEM_PROMPT: &str = r#"You are an expert software engineer helping improve git commit messages. You will receive a YAML representation of a git repository with commit information. Your task is to analyze the commits and suggest improvements to make them follow conventional commit format and best practices.

Rules:
1. Follow conventional commit format: type(scope): description
2. Types: feat, fix, docs, style, refactor, test, chore, ci, build, perf
3. Keep subject lines under 50 characters when possible
4. Use imperative mood ("Add feature" not "Added feature")
5. Provide clear, concise descriptions of what the commit does
6. Only suggest changes for commits that would benefit from improvement
7. Preserve the commit's original intent while improving clarity

Respond with a YAML amendment file in this exact format:
```yaml
amendments:
  - commit: "full-40-character-sha1-hash"
    message: "improved commit message"
  - commit: "another-full-40-character-sha1-hash"  
    message: "another improved commit message"
```"#;

/// Generate user prompt from repository view YAML
pub fn generate_user_prompt(repo_yaml: &str) -> String {
    format!(
        r#"Please analyze the following repository information and suggest commit message improvements:

{}

Focus on commits that:
- Don't follow conventional commit format
- Have unclear or vague descriptions
- Use past tense instead of imperative mood
- Are too verbose or too brief
- Could benefit from proper type/scope classification

Only include commits that actually need improvement. If all commits are already well-formatted, return an empty amendments array."#,
        repo_yaml
    )
}
