#!/usr/bin/env bash
# Asciinema demo: omni-dev Atlassian CLI flow.
set -e

PS=$'\e[1;36m$\e[0m '

p() {
  printf '%s%s\n' "$PS" "$1"
  sleep 0.4
  eval "$1"
  echo
  sleep 0.6
}

p 'cat bug.md'
sleep 0.8

p 'ISSUE=$(omni-dev atlassian jira create bug.md) && echo "Created $ISSUE"'
sleep 0.8

p 'omni-dev atlassian jira read $ISSUE'
sleep 1.2

p 'cat page.md'
sleep 0.8

p 'PAGE=$(omni-dev atlassian confluence create page.md) && echo "Created page $PAGE"'
sleep 0.8

p 'omni-dev atlassian confluence comment add-inline $PAGE --anchor-text "/dashboard" comment.md'
sleep 1.0
