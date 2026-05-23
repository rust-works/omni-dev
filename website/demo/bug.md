---
type: jira
instance: https://rustworks.atlassian.net
project: KAN
issue_type: Bug
summary: Login button unresponsive
labels:
  - auth
  - regression
---

When users click **Login**, the form submits but nothing happens — no redirect, no error.

## Steps to reproduce

1. Open `https://example.com/login`
2. Enter valid credentials
3. Click **Login**

## Expected

User is redirected to `/dashboard`.

## Observed

Form stays on `/login` with no visible feedback. Browser console shows:

```
Uncaught TypeError: Cannot read properties of undefined (reading 'token')
```
