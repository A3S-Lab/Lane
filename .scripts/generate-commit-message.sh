#!/bin/bash

# Script to generate commit message using Claude Code CLI
# Follows Commitizen conventional commit specification

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
MAGENTA='\033[0;35m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

# Emojis
ROCKET="🚀"
CHECK="✅"
CROSS="❌"
WARN="⚠️"
SPARKLES="✨"
GEAR="⚙️"
PENCIL="📝"
MAGNIFY="🔍"
PACKAGE="📦"

# Stage all changes
git add -A

# Get git changes
CHANGES=$(git status --short)

# Check if there are any changes
if [ -z "$CHANGES" ]; then
    echo -e "${DIM}No changes to commit.${RESET}"
    exit 0
fi

# Show what will be committed
echo -e "\n${BOLD}${BLUE}${MAGNIFY} Changes to be committed${RESET}"
echo -e "${DIM}────────────────────────────────────────${RESET}"
git diff --cached --stat
echo ""

# Use Claude Code CLI to generate the message
echo -e "${BOLD}${MAGENTA}${SPARKLES} Generating commit message with AI...${RESET}"

# Count files
FILE_COUNT=$(echo "$CHANGES" | wc -l | xargs)

# Commitizen conventional commit types
CZ_TYPES="feat, fix, docs, style, refactor, perf, test, build, ci, chore, revert"

# Get file change summary
FILE_CHANGES=$(git diff --cached --name-status | head -20)

# Build prompt following Commitizen specification
PROMPT="You are a commit message generator. Analyze these changes and write ONE LINE commit message in Commitizen format.

File changes ($FILE_COUNT files):
$FILE_CHANGES

REQUIRED FORMAT: <type>(<scope>): <subject>

Types (pick ONE): feat, fix, docs, style, refactor, perf, test, build, ci, chore

STRICT RULES:
1. Output ONLY ONE LINE - the commit message itself
2. NO explanations, NO markdown, NO extra text
3. Use imperative mood: \"add\" NOT \"added\" or \"adds\"
4. Subject in lowercase (not title case)
5. No period at end
6. Keep under 72 characters
7. Scope is optional but recommended

EXAMPLES (output format):
feat(api): add user authentication endpoint
fix: resolve memory leak in parser
docs(readme): update installation instructions
chore: upgrade dependencies to latest versions
feat(lane): add priority-based command queue

CRITICAL: Respond with ONLY the commit message line. Nothing else."

# Show loading indicator with modern spinner
show_loading() {
    local pid=$1
    local delay=0.08
    local spinstr='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'
    local temp

    while kill -0 $pid 2>/dev/null; do
        for i in $(seq 0 9); do
            if ! kill -0 $pid 2>/dev/null; then
                break 2
            fi
            local char=${spinstr:$i:1}
            printf "\r  ${CYAN}${char}${RESET} Analyzing changes..."
            sleep $delay
        done
    done
    printf "\r${GREEN}${CHECK}${RESET} Analysis complete     \n"
}

# Try to get AI-generated message
claude -p --print --model haiku "$PROMPT" > /tmp/commit_msg.txt 2>&1 &
CLAUD_PID=$!

# Show loading animation
show_loading $CLAUD_PID

# Wait for result
wait $CLAUD_PID 2>/dev/null

# Extract commit message - take first non-empty line that matches commit format
COMMIT_MSG=$(cat /tmp/commit_msg.txt 2>/dev/null | grep -E '^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\(.+?\))?:.+' | head -1 | xargs)

# If no match, try first non-empty line
if [ -z "$COMMIT_MSG" ]; then
    COMMIT_MSG=$(cat /tmp/commit_msg.txt 2>/dev/null | grep -v '^$' | head -1 | xargs)
fi

rm -f /tmp/commit_msg.txt

# Validate we got a message
if [ -z "$COMMIT_MSG" ] || [[ "$COMMIT_MSG" == *"Error"* ]] || [[ "$COMMIT_MSG" == *"error"* ]]; then
    echo -e "${YELLOW}${WARN}  AI generation failed${RESET}"
    echo ""
    echo -e "${DIM}Please provide a Commitizen-compliant commit message:${RESET}"
    echo -e "${DIM}Format: ${BOLD}<type>(<scope>): <subject>${RESET}"
    echo -e "${DIM}Types: $CZ_TYPES${RESET}"
    echo ""
    read -r COMMIT_MSG
fi

echo ""
echo -e "${BOLD}${CYAN}${PENCIL} Generated commit message${RESET}"
echo -e "${DIM}────────────────────────────────────────${RESET}"
echo -e "${BOLD}${GREEN}$COMMIT_MSG${RESET}"
echo -e "${DIM}────────────────────────────────────────${RESET}"
echo ""

# Ask for confirmation
echo -e "${BOLD}Accept this message?${RESET}"
echo -e "  ${GREEN}[y]${RESET} Yes, commit with this message"
echo -e "  ${RED}[n]${RESET} No, cancel"
echo -e "  ${YELLOW}[e]${RESET} Edit the message"
echo ""
read -p "Your choice (y/n/e): " -n 1 -r
echo
echo ""

if [[ $REPLY =~ ^[Nn]$ ]]; then
    echo -e "${RED}${CROSS} Commit cancelled${RESET}"
    exit 1
elif [[ $REPLY =~ ^[Ee]$ ]]; then
    echo -e "${YELLOW}${PENCIL} Enter custom commit message (Commitizen format):${RESET}"
    echo -e "${DIM}Format: <type>(<scope>): <subject>${RESET}"
    echo ""
    read -r COMMIT_MSG
    echo ""
fi

# Commit with the message
echo -e "${BOLD}${BLUE}${GEAR} Committing changes...${RESET}"
if git commit -m "$COMMIT_MSG"; then
    echo ""
    echo -e "${GREEN}${BOLD}${ROCKET} Changes committed successfully!${RESET}"
    echo ""
    echo -e "${DIM}Commit message: ${RESET}${BOLD}$COMMIT_MSG${RESET}"
    echo ""
    exit 0
else
    echo ""
    echo -e "${RED}${CROSS} Commit failed${RESET}"
    exit 1
fi
