---
type: confluence
instance: https://rustworks.atlassian.net
space_key: SD
title: Login button regression — investigation
---

# Login button regression

Tracking the recent issue where the **Login** button silently fails.

## Expected behaviour

Clicking **Login** with valid credentials should redirect to `/dashboard`.

## Reproduction

1. Open the login page
2. Enter valid credentials
3. Observe: form stays on `/login`, console error

## Suspected cause

The auth token is not being attached to the redirect handler.
