<!-- rp1:start:v0.7.1 -->
## rp1 Knowledge Base

**Use Progressive Disclosure Pattern**

Location: `.rp1/context/`

Files:
- index.md (always load first)
- architecture.md
- modules.md
- patterns.md
- concept_map.md

Loading rules:
1. Always read index.md first.
2. Then load based on task type:
   - Code review: patterns.md
   - Bug investigation: architecture.md, modules.md
   - Feature work: modules.md, patterns.md
   - Strategic or system-wide analysis: all files

## rp1 Skill Awareness

You have access to rp1 skills. When you notice the user working on a task
that an rp1 skill addresses, briefly suggest it.

### Skill Categories
| Category | Skills | Suggest When |
|----------|--------|--------------|
| Development | /build, /build-fast, /speedrun | User starts new feature or describes a change |
| Investigation | /code-investigate | User is debugging or examining errors |
| Quality | /code-check, /code-audit, /code-clean-comments | User finishes implementation |
| Review | /pr-review, /pr-visual, /address-pr-feedback | User prepares or responds to PR |
| Documentation | /write-content, /generate-user-docs | User writes or updates docs |
| Knowledge | /knowledge-build | User needs codebase context or KB is stale |
| Strategy | /strategize, /deep-research, /analyse-security | User faces architectural or security decisions |
| Planning | /blueprint, /blueprint-audit | User plans a project or audits progress |
| Prompt | /prompt-writer, /tersify-prompt | User authors or rewrites prompts |

### Suggestion Rules
- Limit to 1 suggestion per turn. Format: skill name, one sentence why, offer to run.
- Do not re-suggest a skill the user declined this session.
- Do not suggest while an rp1 workflow is already running.
- Only suggest when there is a clear match to the user's current activity.
- For deeper questions about rp1, suggest the user invoke /guide.
<!-- rp1:end:v0.7.1 -->

## 1up MCP Code Discovery

For code-discovery questions in this repo, use the `oneup` MCP tools before broad
raw search. This includes questions like "how does X work", "where is Y handled
or saved", "what calls Z", "where is this pattern implemented", or "what code is
impacted by this change".

Use `oneup_prepare` when index readiness is unknown, `oneup_search` for ranked
discovery, `oneup_read` to hydrate returned handles or locations, `oneup_symbol`
for definition/reference completeness, and `oneup_impact` for likely blast
radius. Use `rg`, `grep`, `find`, shell `1up`, or broad file reads first only for
exact literals, regexes, non-code files, or when the MCP server is unavailable.
