git status
git branch -v
git log --oneline main..HEAD
git push -u origin newhoggy/twiddle-msg-test.md
git diff main...HEAD
gh pr create --title "refactor: restructure twiddle-msg command system" --body "..."