//! Prompt templates and engineering for Claude API

use crate::data::context::{CommitContext, VerbosityLevel, WorkPattern};

/// Default commit guidelines when no project-specific guidelines are provided
const DEFAULT_COMMIT_GUIDELINES: &str = r#"## Commit Message Format

Follow conventional commit format:

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

## Types
- `feat`: New features or enhancements
- `fix`: Bug fixes 
- `docs`: Documentation changes
- `style`: Code style changes (formatting, missing semicolons, etc)
- `refactor`: Code refactoring without changing functionality
- `test`: Adding or updating tests
- `chore`: Maintenance tasks, dependency updates
- `ci`: CI/CD pipeline changes
- `perf`: Performance improvements
- `build`: Changes to build system or external dependencies

## Guidelines
- Use lowercase for description
- No period at the end of description
- Use imperative mood ("add" not "added" or "adds")
- Keep description under 50 characters when possible
- Use body to explain what and why, not how
- Reference issues in footer (e.g., "Fixes #123")
"#;

/// Basic system prompt for commit message improvement (Phase 1 & 2)
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
  - commit: "another-full-40-character-sha1-hash"  
    message: "another improved commit message"

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

/// Legacy alias for backward compatibility
pub const SYSTEM_PROMPT: &str = BASIC_SYSTEM_PROMPT;

/// Generate contextual system prompt based on project and commit context (Phase 3)
pub fn generate_contextual_system_prompt(context: &CommitContext) -> String {
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

    // Add project-specific commit guidelines to system prompt for maximum authority
    if let Some(guidelines) = &context.project.commit_guidelines {
        prompt.push_str("\n\n=== MANDATORY COMMIT MESSAGE TEMPLATE ===");
        prompt.push_str("\nThis is a LITERAL TEMPLATE that you must reproduce EXACTLY.");
        prompt.push_str("\nDo NOT treat this as guidance - it is a FORMAT SPECIFICATION.");
        prompt.push_str("\nEvery character, marker, and structure element must be preserved:");
        prompt.push_str(&format!("\n\n{}", guidelines));
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
        prompt.push_str("\n4. The structure, spacing, and all content must be reproduced verbatim");
        prompt
            .push_str("\n5. Replace only obvious placeholders like <type>, <scope>, <description>");
        prompt.push_str(
            "\n6. Everything else in the template is literal text that must appear in every commit",
        );
        prompt.push_str(
            "\n\nWRONG: Treating the above as 'guidance' and writing conventional commits",
        );
        prompt.push_str(
            "\nRIGHT: Using the above as a literal template and reproducing its exact structure",
        );
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
        prompt.push_str(&format!("\n\nValid scopes for this project:\n{}", scopes));
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
                "\n\nScope consistency: Most changes appear to be in the '{}' scope. Consider using this scope consistently unless files clearly belong to different areas.",
                consistent_scope
            ));
        }
    }

    prompt
}

/// Generate basic user prompt from repository view YAML (Phase 1 & 2)
pub fn generate_user_prompt(repo_yaml: &str) -> String {
    format!(
        r#"Please analyze the following repository information and suggest commit message improvements:

{}

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

CRITICAL: Even if a commit message is perfect and needs no changes, include it in the amendments array with its current message unchanged. This allows users to review all commits and make manual adjustments if desired. DO NOT skip any commits from the amendments array."#,
        repo_yaml
    )
}

/// Generate contextual user prompt with enhanced analysis (Phase 3)
pub fn generate_contextual_user_prompt(repo_yaml: &str, context: &CommitContext) -> String {
    let mut prompt = format!(
        "Please analyze the following repository information and suggest commit message improvements:\n\n{}\n\n",
        repo_yaml
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

    prompt
}

/// System prompt for PR description generation
pub const PR_GENERATION_SYSTEM_PROMPT: &str = r#"You are an expert software engineer helping generate intelligent pull request descriptions. You will receive a YAML representation of a git repository with branch information, commits, and a PR template to fill out.

Your task is to analyze the branch data and generate a comprehensive, well-structured PR description that:
1. Accurately describes what the changes accomplish based on actual code diffs
2. Fills in the provided PR template with relevant information
3. Provides clear value proposition and context for reviewers

Analysis Guidelines:
1. **READ THE DIFF FILES**: Carefully examine the diff content to understand what code changes were actually made
2. **UNDERSTAND THE PURPOSE**: Determine the overall goal and impact of the changes
3. **CATEGORIZE CHANGES**: Identify if this is a new feature, bug fix, refactoring, etc.
4. **ASSESS SCOPE**: Understand which parts of the codebase are affected
5. **HIGHLIGHT VALUE**: Explain why this change is beneficial

Template Filling Instructions:
- Replace placeholder text with specific, accurate information
- Check appropriate boxes based on the actual changes made
- Provide detailed explanations in description sections
- List specific changes made in bullet points
- Include any breaking changes or migration notes if applicable
- Add relevant testing information

CRITICAL RESPONSE FORMAT: You MUST respond with ONLY valid YAML content. Do not include any explanatory text, markdown wrappers, or code blocks. Your entire response must be parseable YAML.

Your response must follow this exact YAML structure:

title: "Concise PR title (50-80 characters ideal)"
description: |
  Your filled-in PR template in markdown format here.
  Include all sections and content as markdown.

DO NOT include any explanatory text, markdown code blocks, or commentary. Start immediately with "title:" and provide only YAML content.

The title should be:
- Concise and descriptive (ideally 50-80 characters)
- Follow conventional commit format when appropriate (e.g., "feat(scope): description")
- Clearly convey the main purpose of the PR

The description should be the complete filled-in PR template ready for GitHub."#;

/// Generate PR description using AI analysis
pub fn generate_pr_description_prompt(repo_yaml: &str, pr_template: &str) -> String {
    format!(
        r#"Please analyze the following repository information and generate a comprehensive pull request description by filling in the provided template:

Repository Information:
{}

PR Template to Fill:
{}

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

Start immediately with "title:" and provide only YAML content. Ensure the title is concise (50-80 characters) and the description contains the complete filled-in template."#,
        repo_yaml, pr_template
    )
}

/// Generate PR system prompt with project context and guidelines
pub fn generate_pr_system_prompt_with_context(
    context: &crate::data::context::CommitContext,
) -> String {
    let mut prompt = PR_GENERATION_SYSTEM_PROMPT.to_string();

    // Add project-specific PR guidelines if available
    if let Some(pr_guidelines) = &context.project.pr_guidelines {
        prompt.push_str("\n\n=== PROJECT PR GUIDELINES ===");
        prompt.push_str("\nThis project has specific guidelines for pull request descriptions:");
        prompt.push_str(&format!("\n\n{}", pr_guidelines));
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

/// Generate PR description prompt with project context
pub fn generate_pr_description_prompt_with_context(
    repo_yaml: &str,
    pr_template: &str,
    context: &crate::data::context::CommitContext,
) -> String {
    let mut prompt = format!(
        r#"Please analyze the following repository information and generate a comprehensive pull request description following the project's specific guidelines:

Repository Information:
{}

PR Template:
{}

"#,
        repo_yaml, pr_template
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
