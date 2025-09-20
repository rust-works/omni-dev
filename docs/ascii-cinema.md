# ASCII Cinema Demo for omni-dev

This guide explains how to create and record compelling ASCII cinema demos showcasing omni-dev's capabilities.

## Quick Start

1. **Install asciinema** (if not already installed):
   ```bash
   # macOS
   brew install asciinema
   
   # Linux
   pip install asciinema
   
   # or
   apt-get install asciinema
   ```

2. **Set up demo environment**:
   ```bash
   # From the project root
   ./scripts/setup-ascii-cinema-demo.sh
   ```

3. **Record the demo**:
   ```bash
   # Start recording
   asciinema rec cinema/omni-dev-demo.cast --title "omni-dev: AI-Powered Git Commit Toolkit"
   
   # Navigate to demo repository
   cd cinema/omni-dev-demo
   
   # Run the demo script
   ../../scripts/run-ascii-cinema-demo.sh
   
   # Press Ctrl+D when done
   ```

4. **Upload to asciinema.org**:
   ```bash
   asciinema upload cinema/omni-dev-demo.cast
   ```

## File Structure

```
omni-dev/
├── scripts/
│   ├── setup-ascii-cinema-demo.sh    # Creates demo repository
│   └── run-ascii-cinema-demo.sh      # Demo presentation script
├── cinema/                           # Created by scripts (ignored by git)
│   ├── omni-dev-demo/               # Demo repository
│   └── *.cast                       # Recording files
└── docs/
    └── ascii-cinema.md              # This documentation
```

## Demo Structure

The demo showcases omni-dev's key features in ~3 minutes:

### 1. **Messy Commit History** (30s)
- Shows realistic bad commit messages ("wip", "fix stuff", "asdf")
- Demonstrates the problem omni-dev solves

### 2. **AI-Powered Transformation** (60s)
- Uses `omni-dev git commit message twiddle` to improve commits
- Shows AI analyzing code changes and generating meaningful messages
- Transforms into conventional commit format

### 3. **Commit Analysis** (30s)
- Demonstrates `omni-dev git commit analyze` for detailed insights
- Shows commit quality metrics and conventional commit validation

### 4. **Branch Analysis** (30s)
- Uses `omni-dev git branch analyze` for overall branch quality
- Displays statistics and quality ratings

### 5. **Professional PR Creation** (45s)
- Shows `omni-dev git branch create pr` generating comprehensive PR descriptions
- Demonstrates AI-powered documentation and testing checklists

## Recording Tips

### Before Recording
- [ ] Ensure omni-dev is installed and working
- [ ] Set up Claude API key: `export CLAUDE_API_KEY="your-key"`
- [ ] Test all commands work in your environment
- [ ] Clear terminal and set comfortable font size
- [ ] Close unnecessary applications

### During Recording
- [ ] Use a clean terminal with good contrast
- [ ] Speak slowly and clearly if adding narration
- [ ] Pause between sections to let viewers absorb
- [ ] Show actual output, not simulated (modify script as needed)

### Terminal Settings
```bash
# Recommended settings for recording
export PS1="$ "  # Simple prompt
stty cols 120 rows 30  # Good dimensions
```

## Customization

### Modify Demo Content
Edit `scripts/setup-ascii-cinema-demo.sh` to:
- Change the project structure
- Add different types of messy commits
- Customize the codebase to your domain

### Adjust Script Timing
Edit `scripts/run-ascii-cinema-demo.sh` to:
- Change typing speed with `type_text` delay parameter
- Modify pause durations
- Add or remove sections

### Alternative Scenarios
Create variations for different audiences:
- **Developer-focused**: More technical details
- **Manager-focused**: Emphasize productivity gains
- **Open-source**: Show collaboration benefits

## Publishing

### Asciinema.org
```bash
# Upload and get shareable URL
asciinema upload cinema/omni-dev-demo.cast

# Embed in README
[![asciicast](https://asciinema.org/a/your-id.svg)](https://asciinema.org/a/your-id)
```

### GitHub README
Add to main README.md:
```markdown
## 🎬 See It In Action

[![asciicast](https://asciinema.org/a/your-id.svg)](https://asciinema.org/a/your-id)

*Watch omni-dev transform messy commits into professional ones with AI-powered analysis*
```

### Social Media
- **Twitter**: Share the asciinema link with key features highlighted
- **LinkedIn**: Professional post about Git workflow improvement
- **Dev.to**: Write article with embedded demo
- **Reddit**: Share in r/programming, r/rust, r/git

## Workflow Commands

```bash
# Complete workflow from project root:

# 1. Setup demo environment
./scripts/setup-ascii-cinema-demo.sh

# 2. Start recording
asciinema rec cinema/omni-dev-demo.cast --title "omni-dev: AI-Powered Git Commit Toolkit"

# 3. Navigate and run demo
cd cinema/omni-dev-demo
../../scripts/run-ascii-cinema-demo.sh

# 4. Stop recording (Ctrl+D)

# 5. Upload
cd ../..
asciinema upload cinema/omni-dev-demo.cast
```

## Troubleshooting

### Common Issues

**Demo script fails**:
- Ensure you're in the demo repository directory (`cinema/omni-dev-demo`)
- Check that omni-dev is installed and in PATH
- Verify Claude API key is set

**Recording quality issues**:
- Increase terminal font size
- Use high contrast color scheme
- Ensure window isn't too small

**Timing issues**:
- Adjust pause durations in script
- Practice running through the demo first
- Consider splitting into multiple shorter recordings

**Scripts not executable**:
```bash
chmod +x scripts/setup-ascii-cinema-demo.sh
chmod +x scripts/run-ascii-cinema-demo.sh
```

### Directory Structure Issues

The `cinema/` directory is automatically created by the setup script and contains:
- Demo repository with messy commits
- Generated `.cast` recording files
- Any temporary files created during recording

This directory is ignored by git to keep the main repository clean.

## Contributing

To improve the demo:
1. Test the scripts on different platforms
2. Suggest additional use cases to showcase
3. Improve the visual presentation
4. Add alternative demo scenarios
5. Update documentation

The goal is to create a compelling, realistic demonstration that shows how omni-dev transforms Git workflows from chaotic to professional.