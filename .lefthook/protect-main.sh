#!/bin/sh
#
# Pre-push hook: block direct pushes to main.
# Checks the current branch name since lefthook doesn't forward stdin.

branch="$(git rev-parse --abbrev-ref HEAD)"

if [ "$branch" = "main" ]; then
  echo ""
  echo "  ╔══════════════════════════════════════════════════════════════╗"
  echo "  ║                                                              ║"
  echo "  ║   ⛔  DANGER: Direct push to main is blocked!               ║"
  echo "  ║                                                              ║"
  echo "  ║   You are attempting to push directly to the main branch.    ║"
  echo "  ║   This branch is protected — all changes must go through     ║"
  echo "  ║   a pull request.                                            ║"
  echo "  ║                                                              ║"
  echo "  ║   Instead:                                                   ║"
  echo "  ║     1. Push to a feature branch                              ║"
  echo "  ║     2. Open a pull request                                   ║"
  echo "  ║     3. Get approval and merge                                ║"
  echo "  ║                                                              ║"
  echo "  ╚══════════════════════════════════════════════════════════════╝"
  echo ""
  exit 1
fi

exit 0
