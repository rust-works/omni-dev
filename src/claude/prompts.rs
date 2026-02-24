//! Prompt templates and engineering for Claude API.

use crate::claude::ai::PromptStyle;
use crate::data::context::{CommitContext, VerbosityLevel, WorkPattern};

/// Default commit guidelines embedded from markdown file at compile time.
/// Used by both twiddle and check commands when no project-specific guidelines are provided.
const DEFAULT_COMMIT_GUIDELINES: &str = include_str!("../templates/default-commit-guidelines.md");

/// Basic system prompt for commit message improvement (Phase 1 & 2).
pub const BASIC_SYSTEM_PROMPT: &str = r#"You are an expert software engineer helping improve git commit messages. You will receive a YAML representation of a git repository with commit information and specific commit message guidelines to follow.

Your task is to analyze the commits and suggest improvements based on:
1. The actual code changes shown in the diff files
2. The commit message guidelines provided in the user prompt

CRITICAL: Your primary focus must be on the ACTUAL CODE CHANGES shown in the diff files. Base your commit messages on what the code actually does, not on file paths, branch names, or assumed context.

Analysis Rules:
1. **MOST IMPORTANT**: Read and analyze the diff_file content to understand what code changes were actually made
   - Look at lines with + (added) and - (removed) to see exactly what changed
   - Identify new functions, modified logic, added features, bug fixes, etc.
   - Focus on WHAT the code does, not WHERE it lives
2. Follow EXACTLY the commit message format and guidelines provided in the user prompt
3. Use imperative mood ("Add feature" not "Added feature") unless guidelines specify otherwise
4. Provide clear, concise descriptions of what the commit actually does (based on code changes)
5. Only suggest changes for commits that would benefit from improvement
6. Preserve the commit's original intent while improving clarity
7. Ignore generic file path patterns - focus on actual functionality changes

DIFF ANALYSIS EXAMPLES:
- Adding debug prints/logging = "debug: add debug logging for X" or "fix: improve error diagnostics for Y"
- Removing validation checks = "fix: allow empty X" or "refactor: remove unnecessary Y validation"  
- Changing error messages = "fix: improve error messages for Z"
- Adding new functionality = "feat: implement X capability"
- Bug fixes = "fix: resolve issue with Y"
- Adding detailed error output + removing validation = "fix: improve error handling and allow edge cases"

SPECIFIC EXAMPLE:
If diff shows:
+ eprintln!("DEBUG: YAML parsing failed...");
+ // Try to provide more helpful error messages
- if self.amendments.is_empty() { bail!("must contain at least one amendment"); }
+ // Empty amendments are allowed - they indicate no changes are needed

This should be: "fix(claude): improve YAML parsing diagnostics and allow empty amendments"
NOT: "feat(client): enhance context handling" (which ignores actual changes)

Analysis Priority:
1. First: What does the code change actually do? (from diff content)
2. Second: How can the message be improved for clarity and accuracy?
3. Third: Apply the exact format specification provided in the user prompt
4. Last: Are there any important implications or impacts to highlight?

CRITICAL OUTPUT REQUIREMENT: You MUST include ALL commits in your response, regardless of whether they need changes or not. If a commit message is already perfect, include it unchanged. Never skip commits from the amendments array.

CRITICAL RESPONSE FORMAT: You MUST respond with ONLY valid YAML content. Do not include any explanatory text, markdown wrappers, or code blocks. Your entire response must be parseable YAML starting immediately with "amendments:" and containing nothing else.

Your response must follow this exact YAML structure:

amendments:
  - commit: "full-40-character-sha1-hash"
    message: "improved commit message"
    summary: "One sentence describing what this commit changes"
  - commit: "another-full-40-character-sha1-hash"
    message: "another improved commit message"
    summary: "One sentence describing what this commit changes"

DO NOT include:
- Any explanatory text before the YAML
- Markdown code blocks (```)
- Commentary or analysis
- Any text after the YAML
- Any non-YAML content whatsoever

Your response must start with "amendments:" and be valid YAML only.

CRITICAL YAML FORMATTING REQUIREMENTS:
1. For single-line messages: Use quoted strings ("message here")
2. For multi-line messages: Use literal block scalar (|) format like this:
   message: |
     subject line here
     
     Body paragraph here with details.
     
     - Bullet point 1
     - Bullet point 2
     - Bullet point 3
     
     Additional paragraphs as needed.

3. NEVER put bullet points or multiple sentences on the same line
4. Use proper indentation and line breaks for readability
5. Leave blank lines between sections for better formatting"#;

/// Legacy alias for backward compatibility.
pub const SYSTEM_PROMPT: &str = BASIC_SYSTEM_PROMPT;

/// Generates a contextual system prompt based on project and commit context (Phase 3).
///
/// # Note
///
/// This is a convenience wrapper that hardcodes [`PromptStyle::Claude`].
/// Use [`generate_contextual_system_prompt_for_provider`] when the active provider is
/// known at the call site (i.e. via [`crate::claude::ai::AiClientMetadata::prompt_style`]).
pub fn generate_contextual_system_prompt(context: &CommitContext) -> String {
    generate_contextual_system_prompt_for_provider(context, PromptStyle::Claude)
}

/// Generates a contextual system prompt with provider-specific handling.
///
/// Provider-specific behaviour is limited to the commit-guidelines section:
/// - [`PromptStyle::Claude`]: frames guidelines as a literal template to reproduce exactly.
/// - [`PromptStyle::OpenAi`]: frames guidelines as guidance to follow, with explicit
///   bullet-point instructions to avoid copying the guideline text verbatim.
///
/// The base prompt (`BASIC_SYSTEM_PROMPT`), verbosity hints, branch context, work-pattern
/// context, and scope-consistency guidance are identical across providers.  Check prompts,
/// coherence prompts, and user prompts are not provider-specific because the observed
/// behavioural difference (template-reproduction vs. guidance-interpretation) only manifests
/// when the guidelines section is present.
pub fn generate_contextual_system_prompt_for_provider(
    context: &CommitContext,
    provider: PromptStyle,
) -> String {
    let mut prompt = BASIC_SYSTEM_PROMPT.to_string();

    // CRITICAL: Emphasize diff analysis priority even with context
    prompt.push_str("\n\n=== CONTEXTUAL INTELLIGENCE GUIDELINES ===");
    prompt.push_str(
        "\nWhile this system has access to project context, branch analysis, and work patterns,",
    );
    prompt.push_str("\nyou MUST still prioritize the actual code changes over contextual hints.");
    prompt.push('\n');
    prompt.push_str("\nContext Usage Priority:");
    prompt.push_str("\n1. PRIMARY: Analyze diff content - what does the code actually do?");
    prompt.push_str(
        "\n2. SECONDARY: Use project context for scope selection and formatting preferences",
    );
    prompt.push_str(
        "\n3. TERTIARY: Use branch context for additional clarity, not as primary message source",
    );
    prompt.push('\n');
    prompt.push_str("\nDO NOT generate commit messages based solely on:");
    prompt.push_str("\n- File paths or directory names");
    prompt.push_str("\n- Branch naming patterns");
    prompt.push_str("\n- Assumed project context");
    prompt.push('\n');
    prompt.push_str("\nALWAYS base messages on what the code changes actually accomplish.");

    // Add verbosity guidance based on change significance
    match context.suggested_verbosity() {
        VerbosityLevel::Comprehensive => {
            prompt.push_str(
                "\n\nFor significant changes, provide comprehensive commit messages with:",
            );
            prompt.push_str("\n- Detailed subject line describing the enhancement");
            prompt.push_str("\n- Multi-paragraph body explaining what was added/changed");
            prompt.push_str("\n- Bulleted lists for complex additions");
            prompt.push_str("\n- Impact statement explaining the significance");
        }
        VerbosityLevel::Detailed => {
            prompt.push_str("\n\nFor moderate changes, provide detailed commit messages with:");
            prompt.push_str("\n- Clear subject line with specific scope");
            prompt.push_str("\n- Multi-paragraph body explaining the change and its purpose");
            prompt.push_str("\n- Bulleted lists for key improvements or additions");
            prompt.push_str("\n- Explain the impact and value of the changes");
        }
        VerbosityLevel::Concise => {
            prompt.push_str("\n\nFor minor changes, focus on clear, concise commit messages.");
        }
    }

    // Add project-specific commit guidelines with provider-specific handling
    if let Some(guidelines) = &context.project.commit_guidelines {
        if provider == PromptStyle::Claude {
            // Claude models handle "literal template" instructions correctly
            prompt.push_str("\n\n=== MANDATORY COMMIT MESSAGE TEMPLATE ===");
            prompt.push_str("\nThis is a LITERAL TEMPLATE that you must reproduce EXACTLY.");
            prompt.push_str("\nDo NOT treat this as guidance - it is a FORMAT SPECIFICATION.");
            prompt.push_str("\nEvery character, marker, and structure element must be preserved:");
            prompt.push_str(&format!("\n\n{guidelines}"));
            prompt.push_str("\n\nCRITICAL TEMPLATE REPRODUCTION RULES:");
            prompt.push_str(
                "\n1. This is NOT a description of how to write commits - it IS the actual format",
            );
            prompt.push_str(
                "\n2. Every element shown above must appear in your commit messages exactly as shown",
            );
            prompt.push_str(
                "\n3. Any text, markers, or symbols in the template are LITERAL and must be included",
            );
            prompt.push_str(
                "\n4. The structure, spacing, and all content must be reproduced verbatim",
            );
            prompt.push_str(
                "\n5. Replace only obvious placeholders like <type>, <scope>, <description>",
            );
            prompt.push_str(
                "\n6. Everything else in the template is literal text that must appear in every commit",
            );
            prompt.push_str(
                "\n\nWRONG: Treating the above as 'guidance' and writing conventional commits",
            );
            prompt.push_str(
                "\nRIGHT: Using the above as a literal template and reproducing its exact structure",
            );
        } else {
            // OpenAI and other models need clearer guidance-based instructions
            prompt.push_str("\n\n=== PROJECT COMMIT GUIDELINES ===");
            prompt.push_str("\nThis project has specific commit guidelines that you MUST follow when improving commit messages.");
            prompt.push_str("\nThese are GUIDELINES for how to write commits, not text to copy:");
            prompt.push_str(&format!("\n\n{guidelines}"));
            prompt.push_str("\n\nCRITICAL GUIDELINES USAGE:");
            prompt.push_str(
                "\n1. These are GUIDELINES that describe how to write commit messages for this project",
            );
            prompt.push_str(
                "\n2. Follow the format, style, and conventions described in the guidelines",
            );
            prompt.push_str("\n3. Use the specified commit types, scopes, and formatting rules");
            prompt.push_str("\n4. Write proper commit messages that follow these guidelines");
            prompt.push_str("\n5. Do NOT copy the guidelines text itself into commit messages");
            prompt.push_str(
                "\n6. Create commit messages that would be approved according to these guidelines",
            );
            prompt.push_str("\n\nWRONG: Copying the guidelines document into the commit message");
            prompt.push_str(
                "\nRIGHT: Writing commit messages that follow the guidelines' format and rules",
            );
        }
    }

    // Add valid scopes if available
    if !context.project.valid_scopes.is_empty() {
        let scopes = context
            .project
            .valid_scopes
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        prompt.push_str(&format!("\n\nValid scopes for this project:\n{scopes}"));
    }

    // Add branch context
    if context.branch.is_feature_branch {
        prompt.push_str(&format!(
            "\n\nBranch context: This is {} on '{}'. Consider this context when improving commit messages.",
            context.branch.work_type,
            context.branch.description
        ));
    }

    // Add work pattern context
    match context.range.work_pattern {
        WorkPattern::Sequential => {
            prompt.push_str("\n\nWork pattern: Sequential feature development. Ensure commit messages show logical progression and build upon each other.");
        }
        WorkPattern::Refactoring => {
            prompt.push_str("\n\nWork pattern: Refactoring work. Focus on clarity about what's being restructured and why. Emphasize improvements in code quality or architecture.");
        }
        WorkPattern::BugHunt => {
            prompt.push_str("\n\nWork pattern: Bug investigation and fixes. Emphasize the problem being solved and the solution approach.");
        }
        WorkPattern::Documentation => {
            prompt.push_str("\n\nWork pattern: Documentation updates. Focus on what documentation was added/improved and its value to users or developers.");
        }
        WorkPattern::Configuration => {
            prompt.push_str("\n\nWork pattern: Configuration changes. Explain what settings were modified and their impact on functionality.");
        }
        WorkPattern::Unknown => {
            // No additional context
        }
    }

    // Add scope consistency guidance
    if let Some(consistent_scope) = &context.range.scope_consistency.consistent_scope {
        if context.range.scope_consistency.confidence > 0.7 {
            prompt.push_str(&format!(
                "\n\nScope consistency: Most changes appear to be in the '{consistent_scope}' scope. Consider using this scope consistently unless files clearly belong to different areas."
            ));
        }
    }

    prompt
}

/// Generates a basic user prompt from repository view YAML (Phase 1 & 2).
pub fn generate_user_prompt(repo_yaml: &str) -> String {
    format!(
        r"Please analyze the following repository information and suggest commit message improvements:

{repo_yaml}

CRITICAL ANALYSIS STEPS:
1. **READ THE DIFF FILES**: For each commit, carefully read the diff_file content to understand exactly what code changes were made
2. **IDENTIFY ACTUAL FUNCTIONALITY**: Determine what the code changes actually accomplish, not what file paths suggest
3. **CHOOSE APPROPRIATE TYPE**: Select commit type (feat/fix/refactor/etc.) based on actual changes, not file locations
4. **SELECT MEANINGFUL SCOPE**: Choose scope based on functionality affected, not just directory names

MANDATORY: Include ALL commits in the amendments array - both those that need improvement AND those that are already well-formatted.

For each commit, analyze whether improvements are needed:
- Check if it follows conventional commit format
- Verify descriptions accurately reflect the actual code changes
- Ensure imperative mood is used (not past tense)
- Confirm verbosity matches the scope of changes made
- Validate type/scope classification based on real functionality
- Ensure messages describe what the code actually does (not generic descriptions)

Remember: File paths and directory names are just hints. The diff content shows the real changes.

CRITICAL: Even if a commit message is perfect and needs no changes, include it in the amendments array with its current message unchanged. This allows users to review all commits and make manual adjustments if desired. DO NOT skip any commits from the amendments array.

SUMMARY FIELD: For each commit, include a `summary` field containing one sentence describing what the commit changes. Keep it factual and brief."
    )
}

/// Generates a contextual user prompt with enhanced analysis (Phase 3).
pub fn generate_contextual_user_prompt(repo_yaml: &str, context: &CommitContext) -> String {
    let mut prompt = format!(
        "Please analyze the following repository information and suggest commit message improvements:\n\n{repo_yaml}\n\n"
    );

    // Commit guidelines are now handled in the system prompt for maximum authority
    // Only show default guidelines if no project-specific ones exist
    if context.project.commit_guidelines.is_none() {
        prompt.push_str("=== COMMIT GUIDELINES ===\n");
        prompt.push_str("Follow these commit guidelines:\n\n");
        prompt.push_str(DEFAULT_COMMIT_GUIDELINES);
        prompt.push_str("\n\n");
    }

    // Emphasize diff analysis even with contextual intelligence
    prompt.push_str("CRITICAL ANALYSIS STEPS (WITH CONTEXT):\n");
    prompt.push_str(
        "1. **READ THE DIFF FILES FIRST**: Understand exactly what code changes were made\n",
    );
    prompt.push_str("2. **IDENTIFY ACTUAL FUNCTIONALITY**: What does the code actually do?\n");
    prompt.push_str("3. **APPLY CONTEXTUAL INTELLIGENCE**: Use project context to enhance accuracy, not replace analysis\n");
    prompt.push_str(
        "4. **SELECT TYPE & SCOPE**: Based on actual changes + project scope definitions\n",
    );
    prompt.push('\n');

    // Add context-specific focus areas
    prompt.push_str("MANDATORY: Include ALL commits in the amendments array - both those needing improvement AND those already well-formatted.\n\n");
    prompt.push_str("For each commit, analyze whether improvements are needed:\n");
    prompt.push_str("- Check if it follows conventional commit format\n");
    prompt.push_str("- Verify descriptions are clear and accurate\n");
    prompt.push_str("- Ensure imperative mood is used (not past tense)\n");

    // Add significance-based criteria
    if context.is_significant_change() {
        prompt.push_str("- Lack sufficient detail for significant changes\n");
        prompt.push_str("- Don't explain the impact or rationale for major modifications\n");
        prompt.push_str("- Miss opportunities to highlight important architectural changes\n");
    } else {
        prompt.push_str("- Are too verbose for simple changes\n");
        prompt.push_str("- Could be more concise while remaining clear\n");
    }

    // Add project-specific focus
    if !context.project.valid_scopes.is_empty() {
        prompt.push_str("- Use incorrect or missing scopes based on file changes\n");
    }

    // Add work pattern specific guidance
    match context.range.work_pattern {
        WorkPattern::Refactoring => {
            prompt.push_str("- Don't clearly explain what was refactored and why\n");
        }
        WorkPattern::Documentation => {
            prompt.push_str("- Don't specify what documentation was improved or added\n");
        }
        WorkPattern::BugHunt => {
            prompt.push_str("- Don't clearly describe the problem being fixed\n");
        }
        _ => {}
    }

    prompt.push_str("\nWhen creating improved messages:\n");

    match context.suggested_verbosity() {
        VerbosityLevel::Comprehensive => {
            prompt.push_str("- Provide comprehensive multi-paragraph commit messages\n");
            prompt.push_str("- Include detailed explanations of changes and their impact\n");
            prompt.push_str("- Use bulleted lists for complex additions\n");
            prompt.push_str("- Add impact statements for significant changes\n");
        }
        VerbosityLevel::Detailed => {
            prompt.push_str("- Provide clear subject lines with detailed explanatory body\n");
            prompt.push_str(
                "- Use multi-paragraph descriptions explaining the changes and their purpose\n",
            );
            prompt.push_str("- Include bulleted lists for key improvements or additions\n");
            prompt.push_str("- Explain the impact and value to users or developers\n");
        }
        VerbosityLevel::Concise => {
            prompt.push_str("- Keep messages concise but descriptive\n");
            prompt.push_str("- Focus on clear, single-line conventional commit format\n");
        }
    }

    prompt.push_str("\nCRITICAL: Include ALL commits in the amendments array. Even if a commit message is perfect and needs no changes, include it with its current message unchanged. This allows users to review all commits and make manual adjustments if desired. DO NOT skip any commits from the amendments array.");
    prompt.push_str(SUMMARY_INSTRUCTION);

    prompt
}

/// System prompt for PR description generation.
pub const PR_GENERATION_SYSTEM_PROMPT: &str = r#"You are a software engineer generating pull request descriptions. You will receive git repository data and a PR template.

Your task:
1. Analyze the code changes in the diff files
2. Fill out the PR template with specific information about what was changed
3. Replace template placeholders with actual details

Analysis steps:
1. Read the diff files to understand what code was changed
2. Determine if this is a new feature, bug fix, or other type of change
3. Fill in the template with accurate information about the changes

RESPONSE FORMAT: Respond with YAML only. No explanations or markdown blocks.

Structure:
title: "Short descriptive title"
description: |
  Filled-in template content here

Requirements:
- Replace all template placeholders with real information
- Check appropriate boxes based on actual changes
- Remove template comments and instructions
- Provide specific details about what was changed"#;

/// Generates a PR description using AI analysis.
pub fn generate_pr_description_prompt(repo_yaml: &str, pr_template: &str) -> String {
    format!(
        r#"Please analyze the following repository information and generate a comprehensive pull request description by filling in the provided template:

Repository Information:
{repo_yaml}

PR Template to Fill:
{pr_template}

INSTRUCTIONS:
1. **ANALYZE THE COMMITS AND DIFFS**: Read through all commits and their diff files to understand exactly what changes were made
2. **UNDERSTAND THE OVERALL PURPOSE**: Determine what this branch accomplishes as a whole
3. **FILL THE TEMPLATE**: Replace placeholder text with specific, accurate information based on your analysis
4. **CHECK APPROPRIATE BOXES**: Mark the correct type of change checkboxes based on actual changes
5. **BE SPECIFIC**: Provide concrete details about what was added, changed, or fixed
6. **EXPLAIN VALUE**: Describe why these changes are beneficial or necessary
7. **LIST CHANGES**: Provide specific bullet points of what was modified
8. **INCLUDE CONTEXT**: Add any relevant background or rationale for the changes

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content. Do not include explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

title: "Your concise PR title here"
description: |
  Your filled-in PR template in markdown format here.

Start immediately with "title:" and provide only YAML content. Ensure the title is concise (50-80 characters) and the description contains the complete filled-in template."#
    )
}

/// Generates a PR system prompt with project context and guidelines.
///
/// # Note
///
/// This is a convenience wrapper that hardcodes [`PromptStyle::Claude`].
/// Use [`generate_pr_system_prompt_with_context_for_provider`] when the active provider is
/// known at the call site (i.e. via [`crate::claude::ai::AiClientMetadata::prompt_style`]).
pub fn generate_pr_system_prompt_with_context(
    context: &crate::data::context::CommitContext,
) -> String {
    generate_pr_system_prompt_with_context_for_provider(context, PromptStyle::Claude)
}

/// Generates a PR system prompt with provider-specific handling.
///
/// Provider-specific behaviour applies to the template-filling instructions:
/// - [`PromptStyle::Claude`]: brief reminder that the PR template is to be filled out,
///   not copied — Claude handles this correctly with minimal instruction.
/// - [`PromptStyle::OpenAi`]: explicit bullet-point instructions with a concrete example
///   of what "fill out" means, because OpenAI models need more explicit guidance to avoid
///   reproducing placeholder text verbatim.
pub fn generate_pr_system_prompt_with_context_for_provider(
    context: &crate::data::context::CommitContext,
    provider: PromptStyle,
) -> String {
    let mut prompt = PR_GENERATION_SYSTEM_PROMPT.to_string();

    // Add provider-specific template handling instructions
    if provider == PromptStyle::Claude {
        prompt.push_str("\n\n=== TEMPLATE HANDLING FOR CLAUDE ===");
        prompt.push_str(
            "\nThe PR template provided is a TEMPLATE TO FILL OUT, not literal text to copy.",
        );
        prompt.push_str(
            "\nYou must REPLACE placeholder content with actual information about the changes.",
        );
    } else {
        prompt.push_str("\n\n=== TEMPLATE FILLING INSTRUCTIONS ===");
        prompt.push_str("\nThe provided PR template should be filled out with specific information about the changes.");
        prompt.push_str("\nReplace placeholder content with actual details:");
        prompt.push_str("\n- Fill in the Description section with what this PR actually does");
        prompt.push_str("\n- Mark the correct Type of Change checkboxes");
        prompt.push_str("\n- List the specific changes made in the Changes Made section");
        prompt.push_str("\n- Remove placeholder text like '(issue_number)' and template comments");
        prompt.push_str("\n- Replace empty bullet points with actual information");
        prompt.push_str("\n\nExample: Instead of '**Core Changes:**\\n-\\n-', write '**Core Changes:**\\n- Added OpenAI API client\\n- Implemented provider-specific prompts'");
    }

    // Add project-specific PR guidelines if available
    if let Some(pr_guidelines) = &context.project.pr_guidelines {
        prompt.push_str("\n\n=== PROJECT PR GUIDELINES ===");
        prompt.push_str("\nThis project has specific guidelines for pull request descriptions:");
        prompt.push_str(&format!("\n\n{pr_guidelines}"));
        prompt.push_str("\n\nIMPORTANT: Follow these project-specific guidelines when generating the PR description.");
        prompt.push_str("\nUse these guidelines to inform the style, level of detail, and specific sections to emphasize.");
    }

    // Add scope information if available
    if !context.project.valid_scopes.is_empty() {
        let scope_names: Vec<&str> = context
            .project
            .valid_scopes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        prompt.push_str(&format!(
            "\n\nValid scopes for this project: {}",
            scope_names.join(", ")
        ));
    }

    prompt
}

/// Generates a PR description prompt with project context.
pub fn generate_pr_description_prompt_with_context(
    repo_yaml: &str,
    pr_template: &str,
    context: &crate::data::context::CommitContext,
) -> String {
    let mut prompt = format!(
        r"Please analyze the following repository information and generate a comprehensive pull request description following the project's specific guidelines:

Repository Information:
{repo_yaml}

PR Template:
{pr_template}

"
    );

    // Add project context information
    if context.project.pr_guidelines.is_some() {
        prompt
            .push_str("IMPORTANT: This project has specific PR guidelines that must be followed. ");
        prompt.push_str("Review the guidelines in the system prompt and apply them to create an appropriate PR description.\n\n");
    }

    // Add branch context if available
    if context.branch.is_feature_branch {
        prompt.push_str(&format!(
            "BRANCH CONTEXT: This is {} work on '{}'. Use this context to better describe the purpose and scope.\n\n",
            context.branch.work_type, context.branch.description
        ));
    }

    prompt.push_str(r#"INSTRUCTIONS:
1. **ANALYZE THE COMMITS AND DIFFS**: Read through all commits and their diff files to understand exactly what changes were made
2. **UNDERSTAND THE OVERALL PURPOSE**: Determine what this branch accomplishes as a whole
3. **FOLLOW PROJECT GUIDELINES**: Apply any project-specific PR guidelines provided in the system prompt
4. **GENERATE COMPREHENSIVE DESCRIPTION**: Create a clear, informative PR description that explains the changes
5. **USE APPROPRIATE DETAIL LEVEL**: Match the level of detail to the significance of the changes
6. **BE SPECIFIC**: Provide concrete details about what was added, changed, or fixed based on actual code changes
7. **EXPLAIN VALUE**: Describe why these changes are beneficial or necessary
8. **REPLACE PLACEHOLDERS**: Remove all placeholder text, comments, and generic content
9. **INCLUDE ACTUAL CHANGES**: List specific bullet points of what was modified based on the diffs

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content. Do not include explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

title: "Your concise PR title here"
description: |
  Your comprehensive PR description in markdown format here.
  Follow project guidelines and replace all template placeholders with actual information.

Start immediately with "title:" and provide only YAML content. The title should follow conventional commit format when appropriate and the description should be tailored to this project's standards."#);

    prompt
}

/// System prompt for commit message check/validation.
pub const CHECK_SYSTEM_PROMPT: &str = r#"You are a commit message reviewer. Your task is to evaluate commit messages against project guidelines and report violations.

You will receive:
1. Project commit guidelines (with severity annotations in a "Severity Levels" section)
2. Commit information including the message and diff

## Severity Levels

The guidelines contain a "Severity Levels" section with a table mapping sections to severities:

```markdown
## Severity Levels

| Severity | Sections                       |
|----------|--------------------------------|
| error    | Format, Subject Line, Accuracy |
| warning  | Content                        |
| info     | Style                          |
```

Meaning:
- `error` = Violations that block CI (exit code 1)
- `warning` = Advisory issues (exit code 0, or 2 with --strict)
- `info` = Suggestions only (never affect exit code)

Sections not listed in the severity table default to `warning`.

## Your Task

For each commit:
1. Check if the message follows each guideline section
2. Compare the message against the actual diff to verify accuracy
3. Report violations with the severity from that section's annotation
4. Suggest a corrected message if there are issues

## Accuracy Checks (Critical)

These are the core value-add checks - compare what the message *claims* against what the diff *shows*:
- Does the commit type match the actual changes? (e.g., don't use `feat` for a bug fix)
- Does the scope match files modified?
- Does the description accurately reflect what was done?
- Are important changes mentioned? (e.g., rate limiting, breaking changes)

## Response Format

CRITICAL: Respond with ONLY valid YAML content. Do not include any explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

checks:
  - commit: "abc123..."
    passes: false
    issues:
      - severity: error
        section: "Subject Line"
        rule: "Keep under 72 characters"
        explanation: "Subject is 85 characters"
      - severity: warning
        section: "Body Guidelines"
        rule: "Body required for large changes"
        explanation: "142 lines changed but no body provided"
    suggestion:
      message: |
        feat(api): add user endpoint

        Implement POST /api/users with validation.
      explanation: |
        - Shortened subject to under 72 chars
        - Added body explaining the change
    summary: "Add REST endpoint for user creation with input validation"

For commits that pass all checks:
  - commit: "def456..."
    passes: true
    issues: []
    summary: "Update CI pipeline to run tests in parallel"

IMPORTANT:
- Include ALL commits in the response, whether they pass or fail
- Use the exact severity from the guidelines' severity table
- Set `passes: true` only if there are no error or warning level issues
- Info-level issues do not affect the `passes` status

## Pre-validated Checks

Some commits include a `pre_validated_checks` field containing deterministic checks already performed. These are AUTHORITATIVE — do not contradict them. For example, if pre_validated_checks says "Scope validity verified: 'cli' is in the valid scopes list", do NOT report an error saying the scope is invalid."#;

/// Generates a check system prompt with project guidelines.
pub fn generate_check_system_prompt(guidelines: Option<&str>) -> String {
    generate_check_system_prompt_with_scopes(guidelines, &[])
}

/// Generates a check system prompt with project guidelines and valid scopes.
pub fn generate_check_system_prompt_with_scopes(
    guidelines: Option<&str>,
    valid_scopes: &[crate::data::context::ScopeDefinition],
) -> String {
    let mut prompt = CHECK_SYSTEM_PROMPT.to_string();

    prompt.push_str("\n\n=== PROJECT COMMIT GUIDELINES ===\n");
    prompt.push_str("Evaluate commits against these guidelines:\n\n");

    if let Some(project_guidelines) = guidelines {
        prompt.push_str(project_guidelines);
    } else {
        prompt.push_str(DEFAULT_COMMIT_GUIDELINES);
    }

    // Add valid scopes if available (ensures check uses same scopes as twiddle)
    if !valid_scopes.is_empty() {
        prompt.push_str("\n\n=== VALID SCOPES FOR THIS PROJECT ===\n");
        prompt.push_str("The following scopes are valid for this project:\n\n");
        for scope in valid_scopes {
            prompt.push_str(&format!("- `{}`: {}\n", scope.name, scope.description));
        }
        prompt.push_str(concat!(
            "\n## Scope Checking Rules\n\n",
            "There are TWO distinct scope checks with different severities:\n\n",
            "1. **Scope Validity** (severity: error, section: \"Accuracy\"): ",
            "The scope is NOT in the valid scopes list above. ",
            "Only report this if pre_validated_checks does NOT confirm the scope is valid.\n\n",
            "2. **Scope Appropriateness** (severity: info, section: \"Scope Appropriateness\"): ",
            "The scope IS in the valid list, but a different valid scope might better match ",
            "the changed files. This is a suggestion only — never an error.\n\n",
            "IMPORTANT: If pre_validated_checks says the scope is valid, you MUST NOT ",
            "report it as an Accuracy error. You may suggest a more appropriate scope at info level.",
        ));
    }

    prompt.push_str("\n\nCRITICAL: Use the Severity Levels table above to determine the severity of each violation. If a section is not listed, default to 'warning'.");

    prompt
}

/// Summary field instruction appended to amendment and check prompts.
const SUMMARY_INSTRUCTION: &str = "\n\nSUMMARY FIELD: For each commit, include a `summary` field containing one sentence describing what the commit changes. This is used for cross-commit coherence analysis. Keep it factual and brief — no diff details, just the functional intent.";

/// Generates a user prompt for the check command.
pub fn generate_check_user_prompt(repo_yaml: &str, include_suggestions: bool) -> String {
    let mut prompt = format!(
        r"Please analyze the following commits and check their messages against the guidelines:

{repo_yaml}

ANALYSIS STEPS:
1. For each commit, read the diff content to understand what was actually changed
2. Compare the commit message against each guideline section
3. Report any violations with appropriate severity level from the guidelines
4. Check accuracy: does the message accurately describe the actual code changes?

"
    );

    if include_suggestions {
        prompt.push_str("SUGGESTIONS: For commits with issues, provide a corrected message suggestion with explanation of improvements.\n\n");
    } else {
        prompt.push_str("SUGGESTIONS: Do NOT include suggestion fields - only report issues.\n\n");
    }

    prompt.push_str(r#"MANDATORY: Include ALL commits in the checks array, whether they pass or fail.

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content starting with "checks:". Do not include any explanatory text, markdown wrappers, or code blocks."#);

    prompt.push_str(SUMMARY_INSTRUCTION);

    prompt
}

/// System prompt for the amendment coherence pass.
///
/// Reviews individually-generated amendments for cross-commit consistency,
/// normalizing scopes, detecting rename chains, and removing redundancy.
pub const AMENDMENT_COHERENCE_SYSTEM_PROMPT: &str = r#"You are an expert software engineer reviewing a set of proposed commit message amendments for cross-commit coherence.

Each commit was analyzed individually. You now see all the proposed messages and summaries together. Your task is to refine the messages for consistency across the entire set.

Review for:
1. **Scope normalization**: If related commits use different scopes for the same area, standardize them
2. **Rename chains**: If commit A renames X→Y and commit B updates references to Y, note the relationship
3. **Redundant descriptions**: Differentiate commits with similar descriptions (e.g., multiple "update dependencies")
4. **Consistent terminology**: Use the same terms for the same concepts across commits
5. **Logical grouping**: Ensure related commits have messages that reflect their relationship

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content. Do not include explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

amendments:
  - commit: "full-40-character-sha1-hash"
    message: |
      type(scope): subject line

      Body paragraph preserved from input.
  - commit: "another-full-40-character-sha1-hash"
    message: "single-line message if input had no body"

IMPORTANT:
- Include ALL commits from the input, even those that need no refinement
- Preserve the original commit hashes exactly
- Only change messages where cross-commit coherence improvements are needed
- Keep changes minimal — do not rewrite messages that are already good
- PRESERVE MESSAGE BODIES: If a message has a multi-line body (subject + blank line + paragraphs), keep the body intact. Only refine the subject line or body wording where cross-commit coherence requires it. Do not strip bodies down to single-line messages.
- Use YAML literal block scalar (|) for multi-line messages
- Your response must start with "amendments:" and be valid YAML only"#;

/// System prompt for the check coherence pass.
///
/// Reviews individually-generated check results for cross-commit consistency.
pub const CHECK_COHERENCE_SYSTEM_PROMPT: &str = r#"You are an expert commit message reviewer examining check results that were generated for individual commits. You now see all results together and should refine them for cross-commit coherence.

Review for:
1. **Scope consistency**: Flag if related commits use inconsistent scopes
2. **Cross-commit accuracy**: Detect if commit messages reference changes in other commits incorrectly
3. **Consistent severity**: Ensure similar issues get the same severity across commits
4. **Missing cross-commit issues**: Flag coordination problems only visible when viewing all commits together

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content. Do not include explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

checks:
  - commit: "abc123..."
    passes: true/false
    issues:
      - severity: error/warning/info
        section: "Section Name"
        rule: "Rule description"
        explanation: "Why this is an issue"
    suggestion:
      message: |
        refined commit message
      explanation: |
        - Reason for refinement

IMPORTANT:
- Include ALL commits from the input
- Preserve original issues — only add cross-commit issues or adjust severity for consistency
- Set `passes: true` only if there are no error or warning level issues
- Your response must start with "checks:" and be valid YAML only"#;

/// System prompt for merging partial chunk amendments into a single commit message.
///
/// When a commit's diff is too large for one AI request, it is split into
/// file-level chunks and each chunk produces a partial amendment. This prompt
/// drives a reduce pass that synthesizes those partial amendments into one
/// cohesive message.
pub(crate) const AMENDMENT_CHUNK_MERGE_SYSTEM_PROMPT: &str = r#"You are an expert software engineer synthesizing commit message proposals that were generated from different subsets of the same commit's file diffs.

Because the commit's diff was too large for a single request, it was split into N chunks and each chunk produced a partial proposed message. You now see all partial messages together with the commit's full `diff --stat` summary and the original commit message.

Your task:
1. **Synthesize** a single cohesive conventional commit message that accurately describes ALL changes in the commit
2. **Choose type and scope** that best represent the overall change — if partial messages disagree, pick the one that covers the broadest set of changes
3. **Preserve details** from each partial message — do not lose important information about what changed
4. **Write a body** when the combined scope warrants it — summarize the key changes from all chunks
5. **Use conventional commit format**: `type(scope): subject` with optional body after a blank line

CRITICAL RESPONSE FORMAT: Respond with ONLY valid YAML content. Do not include explanatory text, markdown wrappers, or code blocks.

Your response must follow this exact YAML structure:

amendments:
  - commit: "full-40-character-sha1-hash"
    message: |
      type(scope): subject line

      Body paragraph synthesized from all chunks.

IMPORTANT:
- Return exactly ONE amendment for this commit
- Preserve the commit hash exactly as given
- Use YAML literal block scalar (|) for multi-line messages
- Your response must start with "amendments:" and be valid YAML only"#;

/// Generates the user prompt for a chunk-merge reduce pass.
///
/// Lists each partial amendment with its index and covered files, plus the
/// original message and full `diff --stat` summary for context.
pub(crate) fn generate_chunk_merge_user_prompt(
    commit_hash: &str,
    original_message: &str,
    diff_summary: &str,
    chunk_amendments: &[crate::data::amendments::Amendment],
) -> String {
    let mut prompt = format!(
        "Merge the following partial amendments for commit {commit_hash} into a single cohesive message.\n\n"
    );

    prompt.push_str(&format!(
        "Original message:\n{}\n\n",
        indent_message(original_message, "  "),
    ));

    prompt.push_str(&format!(
        "Full diff --stat summary:\n{}\n\n",
        indent_message(diff_summary, "  "),
    ));

    prompt.push_str(&format!(
        "Partial amendments ({} chunks):\n\n",
        chunk_amendments.len()
    ));

    for (i, amendment) in chunk_amendments.iter().enumerate() {
        prompt.push_str(&format!(
            "Chunk {}:\n  Proposed message: |\n{}\n\n",
            i + 1,
            indent_message(&amendment.message, "    "),
        ));
    }

    prompt
        .push_str("Synthesize these into a single amendment. Return exactly one amendment entry.");

    prompt
}

/// Generates the user prompt for an amendment coherence pass.
pub fn generate_amendment_coherence_user_prompt(
    items: &[(crate::data::amendments::Amendment, String)],
) -> String {
    let mut prompt = String::from(
        "Review the following individually-generated commit message amendments for cross-commit coherence:\n\n",
    );

    for (i, (amendment, summary)) in items.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. Commit: {}\n   Proposed message: |\n{}\n   Summary: {}\n\n",
            i + 1,
            amendment.commit,
            indent_message(&amendment.message, "     "),
            summary,
        ));
    }

    prompt.push_str("Refine these amendments for cross-commit coherence. Return ALL commits in the amendments array, including those that need no changes.");

    prompt
}

/// Generates the user prompt for a check coherence pass.
pub fn generate_check_coherence_user_prompt(
    items: &[(crate::data::check::CommitCheckResult, String)],
) -> String {
    let mut prompt = String::from(
        "Review the following individually-generated check results for cross-commit coherence:\n\n",
    );

    for (i, (result, summary)) in items.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. Commit: {} ({})\n   Message: {}\n   Passes: {}\n   Issues: {}\n   Summary: {}\n\n",
            i + 1,
            result.hash,
            if result.passes { "PASS" } else { "FAIL" },
            result.message,
            result.passes,
            result.issues.len(),
            summary,
        ));
    }

    prompt.push_str("Refine these check results for cross-commit coherence. Return ALL commits in the checks array.");

    prompt
}

/// Indents each line of a message with the given prefix.
fn indent_message(message: &str, prefix: &str) -> String {
    message
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::context::*;

    fn make_context() -> CommitContext {
        CommitContext {
            project: ProjectContext {
                commit_guidelines: None,
                pr_guidelines: None,
                valid_scopes: Vec::new(),
                ..ProjectContext::default()
            },
            branch: BranchContext {
                work_type: WorkType::Feature,
                description: "add feature".to_string(),
                is_feature_branch: true,
                ..BranchContext::default()
            },
            range: CommitRangeContext::default(),
            files: Vec::new(),
            user_provided: None,
        }
    }

    // ── indent_message ─────────────────────────────────────────────

    #[test]
    fn indent_single_line() {
        assert_eq!(indent_message("hello", "  "), "  hello");
    }

    #[test]
    fn indent_multiple_lines() {
        let result = indent_message("line1\nline2\nline3", ">> ");
        assert_eq!(result, ">> line1\n>> line2\n>> line3");
    }

    #[test]
    fn indent_empty_string() {
        // "".lines() yields zero items, so join produces ""
        assert_eq!(indent_message("", "  "), "");
    }

    // ── generate_user_prompt ───────────────────────────────────────

    #[test]
    fn user_prompt_contains_yaml() {
        let prompt = generate_user_prompt("commits:\n  - hash: abc123");
        assert!(prompt.contains("commits:\n  - hash: abc123"));
        assert!(prompt.contains("CRITICAL ANALYSIS STEPS"));
    }

    #[test]
    fn user_prompt_requires_all_commits() {
        let prompt = generate_user_prompt("test");
        assert!(prompt.contains("Include ALL commits"));
    }

    // ── generate_contextual_system_prompt ──────────────────────────

    #[test]
    fn contextual_system_prompt_contains_base() {
        let context = make_context();
        let prompt = generate_contextual_system_prompt(&context);
        assert!(prompt.contains("expert software engineer"));
        assert!(prompt.contains("CONTEXTUAL INTELLIGENCE GUIDELINES"));
    }

    #[test]
    fn contextual_system_prompt_with_branch_context() {
        let context = make_context();
        let prompt = generate_contextual_system_prompt(&context);
        assert!(prompt.contains("add feature"));
    }

    #[test]
    fn contextual_system_prompt_with_guidelines_claude() {
        let mut context = make_context();
        context.project.commit_guidelines = Some("Use type(scope): desc".to_string());
        let prompt = generate_contextual_system_prompt_for_provider(&context, PromptStyle::Claude);
        assert!(prompt.contains("MANDATORY COMMIT MESSAGE TEMPLATE"));
        assert!(prompt.contains("Use type(scope): desc"));
    }

    #[test]
    fn contextual_system_prompt_with_guidelines_openai() {
        let mut context = make_context();
        context.project.commit_guidelines = Some("Use type(scope): desc".to_string());
        let prompt = generate_contextual_system_prompt_for_provider(&context, PromptStyle::OpenAi);
        assert!(prompt.contains("PROJECT COMMIT GUIDELINES"));
        assert!(!prompt.contains("MANDATORY COMMIT MESSAGE TEMPLATE"));
    }

    #[test]
    fn contextual_system_prompt_with_scopes() {
        let mut context = make_context();
        context.project.valid_scopes = vec![ScopeDefinition {
            name: "cli".to_string(),
            description: "CLI module".to_string(),
            examples: Vec::new(),
            file_patterns: Vec::new(),
        }];
        let prompt = generate_contextual_system_prompt(&context);
        assert!(prompt.contains("cli: CLI module"));
    }

    #[test]
    fn contextual_system_prompt_work_pattern_refactoring() {
        let mut context = make_context();
        context.range.work_pattern = WorkPattern::Refactoring;
        let prompt = generate_contextual_system_prompt(&context);
        assert!(prompt.contains("Refactoring work"));
    }

    #[test]
    fn contextual_system_prompt_scope_consistency() {
        let mut context = make_context();
        context.range.scope_consistency = ScopeAnalysis {
            consistent_scope: Some("cli".to_string()),
            scope_changes: vec!["cli".to_string()],
            confidence: 0.9,
        };
        let prompt = generate_contextual_system_prompt(&context);
        assert!(prompt.contains("'cli' scope"));
    }

    // ── generate_contextual_user_prompt ────────────────────────────

    #[test]
    fn contextual_user_prompt_no_guidelines_uses_default() {
        let context = make_context();
        let prompt = generate_contextual_user_prompt("yaml-data", &context);
        assert!(prompt.contains("COMMIT GUIDELINES"));
    }

    #[test]
    fn contextual_user_prompt_with_guidelines_skips_default() {
        let mut context = make_context();
        context.project.commit_guidelines = Some("custom guidelines".to_string());
        let prompt = generate_contextual_user_prompt("yaml-data", &context);
        assert!(!prompt.contains("=== COMMIT GUIDELINES ==="));
    }

    #[test]
    fn contextual_user_prompt_significant_change() {
        let mut context = make_context();
        context.range.architectural_impact = ArchitecturalImpact::Breaking;
        let prompt = generate_contextual_user_prompt("yaml-data", &context);
        assert!(prompt.contains("significant changes"));
    }

    // ── generate_check_system_prompt ───────────────────────────────

    #[test]
    fn check_system_prompt_default_guidelines() {
        let prompt = generate_check_system_prompt(None);
        assert!(prompt.contains("commit message reviewer"));
        assert!(prompt.contains("PROJECT COMMIT GUIDELINES"));
    }

    #[test]
    fn check_system_prompt_custom_guidelines() {
        let prompt = generate_check_system_prompt(Some("My custom rules"));
        assert!(prompt.contains("My custom rules"));
    }

    #[test]
    fn check_system_prompt_with_scopes() {
        let scopes = vec![ScopeDefinition {
            name: "cli".to_string(),
            description: "CLI module".to_string(),
            examples: Vec::new(),
            file_patterns: Vec::new(),
        }];
        let prompt = generate_check_system_prompt_with_scopes(None, &scopes);
        assert!(prompt.contains("VALID SCOPES"));
        assert!(prompt.contains("`cli`: CLI module"));
    }

    // ── generate_check_user_prompt ─────────────────────────────────

    #[test]
    fn check_user_prompt_with_suggestions() {
        let prompt = generate_check_user_prompt("yaml-data", true);
        assert!(prompt.contains("provide a corrected message"));
    }

    #[test]
    fn check_user_prompt_without_suggestions() {
        let prompt = generate_check_user_prompt("yaml-data", false);
        assert!(prompt.contains("Do NOT include suggestion"));
    }

    // ── generate_pr_description_prompt ─────────────────────────────

    #[test]
    fn pr_description_prompt_contains_template() {
        let prompt = generate_pr_description_prompt("repo-yaml", "## Description\n");
        assert!(prompt.contains("repo-yaml"));
        assert!(prompt.contains("## Description"));
    }

    // ── generate_pr_system_prompt_with_context ─────────────────────

    #[test]
    fn pr_system_prompt_claude_provider() {
        let context = make_context();
        let prompt =
            generate_pr_system_prompt_with_context_for_provider(&context, PromptStyle::Claude);
        assert!(prompt.contains("TEMPLATE HANDLING FOR CLAUDE"));
    }

    #[test]
    fn pr_system_prompt_openai_provider() {
        let context = make_context();
        let prompt =
            generate_pr_system_prompt_with_context_for_provider(&context, PromptStyle::OpenAi);
        assert!(prompt.contains("TEMPLATE FILLING INSTRUCTIONS"));
    }

    #[test]
    fn pr_system_prompt_with_pr_guidelines() {
        let mut context = make_context();
        context.project.pr_guidelines = Some("Always include screenshots".to_string());
        let prompt = generate_pr_system_prompt_with_context(&context);
        assert!(prompt.contains("PROJECT PR GUIDELINES"));
        assert!(prompt.contains("Always include screenshots"));
    }

    // ── generate_pr_description_prompt_with_context ─────────────────

    #[test]
    fn pr_description_with_context_branch_info() {
        let context = make_context();
        let prompt = generate_pr_description_prompt_with_context("yaml", "template", &context);
        assert!(prompt.contains("BRANCH CONTEXT"));
        assert!(prompt.contains("add feature"));
    }

    // ── coherence prompts ──────────────────────────────────────────

    #[test]
    fn amendment_coherence_prompt_lists_commits() {
        use crate::data::amendments::Amendment;

        let items = vec![
            (
                Amendment::new("a".repeat(40), "feat(cli): add flag".to_string()),
                "Add CLI flag".to_string(),
            ),
            (
                Amendment::new("b".repeat(40), "fix(git): fix bug".to_string()),
                "Fix git bug".to_string(),
            ),
        ];
        let prompt = generate_amendment_coherence_user_prompt(&items);
        assert!(prompt.contains(&"a".repeat(40)));
        assert!(prompt.contains(&"b".repeat(40)));
        assert!(prompt.contains("Add CLI flag"));
        assert!(prompt.contains("Fix git bug"));
    }

    #[test]
    fn check_coherence_prompt_lists_results() {
        use crate::data::check::CommitCheckResult;

        let items = vec![(
            CommitCheckResult {
                hash: "a".repeat(40),
                message: "feat: test".to_string(),
                passes: true,
                issues: Vec::new(),
                suggestion: None,
                summary: None,
            },
            "Test commit".to_string(),
        )];
        let prompt = generate_check_coherence_user_prompt(&items);
        assert!(prompt.contains(&"a".repeat(40)));
        assert!(prompt.contains("PASS"));
        assert!(prompt.contains("Test commit"));
    }

    // ── constants ──────────────────────────────────────────────────

    #[test]
    fn basic_system_prompt_not_empty() {
        assert!(BASIC_SYSTEM_PROMPT.len() > 100);
        assert!(BASIC_SYSTEM_PROMPT.contains("amendments:"));
    }

    #[test]
    fn pr_generation_system_prompt_not_empty() {
        assert!(PR_GENERATION_SYSTEM_PROMPT.len() > 100);
        assert!(PR_GENERATION_SYSTEM_PROMPT.contains("pull request"));
    }

    #[test]
    fn check_system_prompt_constant_not_empty() {
        assert!(CHECK_SYSTEM_PROMPT.len() > 100);
        assert!(CHECK_SYSTEM_PROMPT.contains("commit message reviewer"));
    }

    #[test]
    fn amendment_coherence_system_prompt_not_empty() {
        assert!(AMENDMENT_COHERENCE_SYSTEM_PROMPT.len() > 100);
        assert!(AMENDMENT_COHERENCE_SYSTEM_PROMPT.contains("coherence"));
    }

    #[test]
    fn check_coherence_system_prompt_not_empty() {
        assert!(CHECK_COHERENCE_SYSTEM_PROMPT.len() > 100);
    }

    #[test]
    fn chunk_merge_system_prompt_not_empty() {
        assert!(AMENDMENT_CHUNK_MERGE_SYSTEM_PROMPT.len() > 100);
        assert!(AMENDMENT_CHUNK_MERGE_SYSTEM_PROMPT.contains("amendments:"));
    }

    // ── chunk merge user prompt ──────────────────────────────────

    #[test]
    fn chunk_merge_user_prompt_includes_all_chunks() {
        use crate::data::amendments::Amendment;

        let chunks = vec![
            Amendment::new("a".repeat(40), "feat(cli): add flag".to_string()),
            Amendment::new("a".repeat(40), "feat(cli): add option".to_string()),
        ];
        let prompt = generate_chunk_merge_user_prompt(
            &"a".repeat(40),
            "original message",
            " src/main.rs | 10 ++++",
            &chunks,
        );
        assert!(prompt.contains("Chunk 1:"));
        assert!(prompt.contains("Chunk 2:"));
        assert!(prompt.contains("add flag"));
        assert!(prompt.contains("add option"));
        assert!(prompt.contains("2 chunks"));
    }

    #[test]
    fn chunk_merge_user_prompt_includes_diff_summary() {
        use crate::data::amendments::Amendment;

        let chunks = vec![Amendment::new("a".repeat(40), "feat: change".to_string())];
        let summary = " src/main.rs | 10 ++++\n src/lib.rs  |  5 ++";
        let prompt =
            generate_chunk_merge_user_prompt(&"a".repeat(40), "original", summary, &chunks);
        assert!(prompt.contains("src/main.rs | 10"));
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("original"));
    }

    #[test]
    fn system_prompt_alias() {
        assert_eq!(SYSTEM_PROMPT, BASIC_SYSTEM_PROMPT);
    }
}
