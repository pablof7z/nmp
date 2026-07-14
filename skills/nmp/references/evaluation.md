# Evaluation protocol

Use [raw evaluation prompts](evaluation-prompts.md) after every material skill revision. Keep task-specific expected answers and previous outputs out of the context given to the fresh agent.

For each run:

1. Record the exact repository and skill commits before dispatch.
2. Give one raw prompt to a new agent that has not seen prior responses or review criteria.
3. Preserve the raw response or a durable, source-linked summary in the governing GitHub issue or pull request, outside this agent-facing package.
4. After the response is complete, verify every named type, method, field, error, build command, and lifecycle claim against the [source map](source-map.md).
5. Record unsupported API inventions, missed platform gaps, unsafe lifecycle advice, and whether the plan remains implementable.
6. Revise the skill for any material finding, then use a different raw prompt with another fresh agent.

Do not count a run as blind if the agent saw task-specific expected answers, a previous response, or a prompt-specific rubric. A polished response without current-source verification is not evidence.
