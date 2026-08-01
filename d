gh api -X PUT repos/AndreaZanellini/glyde/branches/main/protection \
  --input - << 'EOF'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "Format & Clippy (windows-latest)",
      "Format & Clippy (macos-14)",
      "Format & Clippy (ubuntu-latest)", 
      "Architecture guard",
      "Test core (ubuntu-latest)",
      "Test core (macos-14)",
      "Test core (windows-latest)",
      "GUI build & test (ubuntu-latest)",
      "GUI build & test (macos-14)",
      "GUI build & test (windows-latest)",
      "Performance gates",
      "Licenses & advisories"
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "required_approving_review_count": 0
  },
  "restrictions": null
}
EOF