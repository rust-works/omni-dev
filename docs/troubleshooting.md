# Troubleshooting Guide

Common issues and solutions when using omni-dev.

## Table of Contents

1. [Installation Issues](#installation-issues)
2. [API Key Problems](#api-key-problems)
3. [Configuration Issues](#configuration-issues)
4. [Commit Analysis Problems](#commit-analysis-problems)
5. [Performance Issues](#performance-issues)
6. [Git Repository Issues](#git-repository-issues)
7. [Command Line Issues](#command-line-issues)
8. [Getting Help](#getting-help)

## Installation Issues

### Error: `cargo install omni-dev` fails

**Symptom**: Installation fails with compilation errors.

**Common Causes & Solutions**:

1. **Rust Version Too Old**

   ```bash
   # Check Rust version
   rustc --version
   
   # Update Rust (need 1.70+)
   rustup update
   ```

2. **Missing System Dependencies**

   ```bash
   # macOS
   xcode-select --install
   
   # Ubuntu/Debian
   sudo apt update
   sudo apt install build-essential pkg-config libssl-dev
   
   # CentOS/RHEL
   sudo yum groupinstall "Development Tools"
   sudo yum install openssl-devel
   ```

3. **Network Issues**

   ```bash
   # Use alternative registry
   cargo install omni-dev --registry crates-io
   
   # Or build from source
   git clone https://github.com/rust-works/omni-dev.git
   cd omni-dev
   cargo build --release
   ```

### Error: `omni-dev: command not found`

**Symptom**: Command not found after installation.

**Solution**: Add Cargo bin directory to PATH:

```bash
# Add to your shell profile (.bashrc, .zshrc, etc.)
export PATH="$HOME/.cargo/bin:$PATH"

# Reload shell or run:
source ~/.bashrc  # or ~/.zshrc
```

## API Key Problems

### Error: `CLAUDE_API_KEY not found`

**Symptom**:

```
Error: Claude API key not found
  Caused by: API key not found
```

**Solutions**:

1. **Set Environment Variable**

   ```bash
   export CLAUDE_API_KEY="your-api-key-here"
   
   # Make permanent
   echo 'export CLAUDE_API_KEY="your-key"' >> ~/.bashrc
   source ~/.bashrc
   ```

2. **Verify Key Format**

   ```bash
   # Should start with "sk-ant-api03-"
   echo $CLAUDE_API_KEY | head -c 20
   ```

3. **Check for Hidden Characters**

   ```bash
   # Remove whitespace/newlines
   export CLAUDE_API_KEY="$(echo $CLAUDE_API_KEY | tr -d '[:space:]')"
   ```

### Error: `API request failed: HTTP 401`

**Symptom**: Authentication failed.

**Causes & Solutions**:

1. **Invalid API Key**
   - Get new key from [Anthropic Console](https://console.anthropic.com/)
   - Ensure key is active and not expired

2. **Wrong Key Format**

   ```bash
   # API key should look like:
   sk-ant-api03-abcd1234...
   ```

3. **Account Issues**
   - Check account status at Anthropic Console
   - Ensure billing/credits are available

### Error: `API request failed: HTTP 429`

**Symptom**: Rate limited.

**Solutions**:

1. **Reduce Batch Size**

   ```bash
   # Use smaller batches
   omni-dev git commit message twiddle 'HEAD~10..HEAD' --batch-size 1
   ```

2. **Wait and Retry**
   - Wait a few minutes between large requests
   - Rate limits reset over time

3. **Upgrade API Tier**
   - Check rate limits in Anthropic Console
   - Consider upgrading account tier

## Configuration Issues

### Error: Scopes not detected

**Symptom**: omni-dev doesn't use project-specific scopes.

**Debugging Steps**:

1. **Check Directory Structure**

   ```bash
   ls -la .omni-dev/
   # Should show: scopes.yaml, commit-guidelines.md
   ```

2. **Validate YAML Syntax**

   ```bash
   # Check YAML is valid
   python -c "import yaml; yaml.safe_load(open('.omni-dev/scopes.yaml'))"
   
   # Or use online YAML validator
   ```

3. **Test File Patterns**

   ```bash
   # See what files changed in commit
   git show --name-only HEAD
   
   # Check if patterns match
   grep -A5 "file_patterns" .omni-dev/scopes.yaml
   ```

4. **Use Absolute Context Directory**

   ```bash
   # Specify full path
   omni-dev git commit message twiddle 'HEAD~3..HEAD' --context-dir "$(pwd)/.omni-dev"
   ```

### Error: Context directory not found

**Symptom**:

```
Context directory not found: .omni-dev/
```

**Solutions**:

1. **Create Directory**

   ```bash
   mkdir .omni-dev
   ```

2. **Check Current Directory**

   ```bash
   # Must be in git repository root
   pwd
   git rev-parse --show-toplevel  # Should match
   ```

3. **Use Custom Directory**

   ```bash
   # If config is elsewhere
   omni-dev git commit message twiddle 'HEAD~3..HEAD' --context-dir ./config
   ```

### Error: YAML parsing failed

**Symptom**: Configuration file syntax errors.

**Solutions**:

1. **Check YAML Syntax**

   ```bash
   # Common issues:
   # - Tabs instead of spaces
   # - Missing quotes around strings with special chars
   # - Incorrect indentation
   
   # Validate with Python
   python -c "import yaml; print(yaml.safe_load(open('.omni-dev/scopes.yaml')))"
   ```

2. **Fix Common Issues**

   ```yaml
   # ❌ Bad - tabs used
   scopes:
    - name: "api"
   
   # ✅ Good - spaces used
   scopes:
     - name: "api"
   
   # ❌ Bad - unquoted string with colon
   - name: api: endpoints
   
   # ✅ Good - quoted string
   - name: "api: endpoints"
   ```

## Commit Analysis Problems

### Error: `Not in a git repository`

**Symptom**:

```
Error: Failed to open git repository
  Caused by: Not in a git repository
```

**Solutions**:

1. **Check Git Repository**

   ```bash
   git status  # Should work
   
   # If not a git repo:
   git init
   ```

2. **Check Working Directory**

   ```bash
   # Must be inside git repository
   cd /path/to/your/git/repo
   omni-dev git commit message twiddle 'HEAD~3..HEAD'
   ```

### Error: `Working directory is not clean`

**Symptom**:

```
Error: Cannot amend commits with uncommitted changes
  Caused by: Working directory is not clean
```

**Solutions**:

1. **Commit Changes**

   ```bash
   git add .
   git commit -m "temp commit"
   ```

2. **Stash Changes**

   ```bash
   git stash push -m "temp stash"
   # After omni-dev: git stash pop
   ```

3. **Use View Instead of Twiddle**

   ```bash
   # View doesn't require clean directory
   omni-dev git commit message view 'HEAD~3..HEAD'
   ```

### Error: Invalid commit range

**Symptom**:

```
Error: Invalid commit range: HEAD~100..HEAD
```

**Solutions**:

1. **Check Available Commits**

   ```bash
   # See how many commits exist
   git log --oneline | wc -l
   ```

2. **Use Valid Range**

   ```bash
   # If only 5 commits exist:
   omni-dev git commit message twiddle 'HEAD~5..HEAD'
   
   # Or use specific hashes:
   omni-dev git commit message twiddle 'abc123..def456'
   ```

3. **Check Branch History**

   ```bash
   git log --oneline -10  # See recent commits
   ```

### Error: No commits found in range

**Symptom**: Empty commit range or no commits to analyze.

**Solutions**:

1. **Verify Commit Range**

   ```bash
   # Check what's in range
   git log --oneline 'HEAD~3..HEAD'
   ```

2. **Use Different Range**

   ```bash
   # Compare to main branch
   omni-dev git commit message twiddle 'origin/main..HEAD'
   
   # Or use absolute range
   omni-dev git commit message twiddle 'HEAD~5..HEAD'
   ```

## Performance Issues

### Issue: Slow processing with large commit ranges

**Symptom**: omni-dev takes a long time with many commits.

**Solutions**:

1. **Use Smaller Batch Sizes**

   ```bash
   # Process 2 commits at a time
   omni-dev git commit message twiddle 'HEAD~20..HEAD' --batch-size 2
   ```

2. **Process in Stages**

   ```bash
   # Break up large ranges
   omni-dev git commit message twiddle 'HEAD~10..HEAD~5'
   omni-dev git commit message twiddle 'HEAD~5..HEAD'
   ```

3. **Save and Review**

   ```bash
   # Save suggestions first, then apply
   omni-dev git commit message twiddle 'HEAD~20..HEAD' --save-only suggestions.yaml
   omni-dev git commit message amend suggestions.yaml
   ```

### Issue: API timeouts

**Symptom**: Requests timing out or failing.

**Solutions**:

1. **Reduce Batch Size**

   ```bash
   omni-dev git commit message twiddle 'HEAD~10..HEAD' --batch-size 1
   ```

2. **Retry with Exponential Backoff**

   ```bash
   # Wait between retries
   sleep 30
   omni-dev git commit message twiddle 'HEAD~5..HEAD'
   ```

## Git Repository Issues

### Error: Cannot amend non-HEAD commits

**Symptom**: Trying to amend commits that aren't the latest.

**Expected Behavior**: omni-dev uses interactive rebase for non-HEAD commits.

**If Problems Occur**:

1. **Ensure Clean Working Directory**

   ```bash
   git status  # Should be clean
   ```

2. **Check Interactive Rebase Setup**

   ```bash
   # Set git editor if needed
   git config --global core.editor "nano"
   # or vim, code --wait, etc.
   ```

3. **Manual Rebase if Needed**

   ```bash
   # Do interactive rebase manually
   git rebase -i HEAD~5
   # Edit commit messages as needed
   ```

### Error: Remote branch not found

**Symptom**: Can't find origin/main or base branch.

**Solutions**:

1. **Check Remote Branches**

   ```bash
   git branch -r  # See remote branches
   ```

2. **Update Remote References**

   ```bash
   git fetch origin
   ```

3. **Use Correct Branch Name**

   ```bash
   # If main branch is 'master':
   omni-dev git commit message twiddle 'origin/master..HEAD'
   ```

### Error: Merge conflicts during rebase

**Symptom**: Interactive rebase fails with conflicts.

**Solutions**:

1. **Resolve Conflicts Manually**

   ```bash
   # Edit conflicted files
   git add .
   git rebase --continue
   ```

2. **Abort and Try Different Approach**

   ```bash
   git rebase --abort
   
   # Use smaller commit ranges
   omni-dev git commit message twiddle 'HEAD~3..HEAD'
   ```

## Command Line Issues

### Error: Unknown argument

**Symptom**:

```
error: unexpected argument '--unknown-flag' found
```

**Solutions**:

1. **Check Available Options**

   ```bash
   omni-dev git commit message twiddle --help
   ```

2. **Use Correct Flag Names**

   ```bash
   # Common correct flags:
   --use-context
   --batch-size 4
   --auto-apply
   --save-only file.yaml
   --context-dir ./config
   ```

### Error: Invalid commit range format

**Symptom**: Git range syntax errors.

**Valid Formats**:

```bash
# ✅ Valid ranges:
'HEAD~5..HEAD'          # Last 5 commits
'origin/main..HEAD'     # Current branch vs main
'abc123..def456'        # Between specific commits
'HEAD^..HEAD'           # Just last commit

# ❌ Invalid:
HEAD~5..HEAD           # Missing quotes
'HEAD-5..HEAD'         # Wrong syntax (-5 instead of ~5)
'HEAD..HEAD~5'         # Backwards range
```

### Issue: Quotes and Shell Escaping

**Symptom**: Shell interpreting range characters incorrectly.

**Solutions**:

```bash
# ✅ Always quote commit ranges:
omni-dev git commit message twiddle 'HEAD~5..HEAD'

# ✅ Or escape special characters:
omni-dev git commit message twiddle HEAD~5..HEAD

# On Windows Command Prompt:
omni-dev git commit message twiddle "HEAD~5..HEAD"
```

## Getting Help

### Enable Debug Output

For detailed troubleshooting information:

```bash
# Set Rust log level for more details
RUST_LOG=debug omni-dev git commit message twiddle 'HEAD~3..HEAD' --use-context

# Or just errors:
RUST_LOG=error omni-dev git commit message twiddle 'HEAD~3..HEAD'
```

### Collect System Information

When reporting issues, include:

```bash
# System info
uname -a
rustc --version
cargo --version

# Git info  
git --version
git status
git log --oneline -5

# omni-dev info
omni-dev --version

# Configuration
ls -la .omni-dev/
echo "API key length: $(echo $CLAUDE_API_KEY | wc -c)"
```

### Test with Minimal Example

Create a minimal reproduction:

```bash
# Create test repo
mkdir test-omni-dev
cd test-omni-dev
git init

# Create test commits
echo "first" > file.txt
git add file.txt
git commit -m "first commit"

echo "second" > file.txt
git add file.txt  
git commit -m "second commit"

# Test omni-dev
omni-dev git commit message twiddle 'HEAD^..HEAD' --use-context
```

### Common Solutions Checklist

Before asking for help, verify:

- [ ] omni-dev is latest version: `cargo install omni-dev`
- [ ] CLAUDE_API_KEY is set correctly
- [ ] In a git repository: `git status` works
- [ ] Working directory is clean (for twiddle command)
- [ ] Commit range is valid: `git log --oneline 'HEAD~5..HEAD'`
- [ ] Configuration syntax is correct (if using `.omni-dev/`)

## Support Channels

### GitHub Issues

For bugs and feature requests: <https://github.com/rust-works/omni-dev/issues>

**Include in Bug Reports**:

- omni-dev version: `omni-dev --version`
- Rust version: `rustc --version`  
- Operating system
- Complete error message
- Steps to reproduce
- Minimal example if possible

### GitHub Discussions  

For questions and general help: <https://github.com/rust-works/omni-dev/discussions>

### Community Support

- Tag questions with `omni-dev` on Stack Overflow
- Join Rust community channels and ask about Git tools

### Documentation

- [User Guide](user-guide.md) - Complete usage guide
- [Configuration Guide](configuration.md) - Setup instructions
- [Examples](examples.md) - Real-world examples

## Frequently Asked Questions

### Q: Can I use omni-dev without Claude API key?

**A**: No, the AI-powered features require a Claude API key. However, you
can use the `view` command to analyze commits without AI suggestions.

### Q: Does omni-dev modify my git history?

**A**: Only when you explicitly approve changes. The `view` command is
read-only. The `twiddle` command shows you proposed changes and asks for
confirmation before applying.

### Q: Can I undo changes made by omni-dev?

**A**: Yes, git tracks all changes:

```bash
# See recent changes
git reflog

# Undo last change
git reset --hard HEAD@{1}
```

### Q: Is it safe to use on shared/public repositories?

**A**: Yes, but be careful:

- Always review changes before applying
- Don't rewrite history on shared branches
- Consider using `--save-only` for review workflows

### Q: How much does the Claude API cost?

**A**: Check current pricing at
[Anthropic Pricing](https://anthropic.com/pricing). Typical usage for commit
message improvement is very low cost.

### Q: Can I use this in CI/CD pipelines?

**A**: Yes, but consider:

- Store API key as secure secret
- Use `--auto-apply` for automated workflows
- Test thoroughly in development first
- Be mindful of API rate limits
