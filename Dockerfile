# For Glama (glama.ai) MCP introspection only: builds an image that starts the
# sqlike MCP server over stdio so Glama can run `initialize` + `tools/list`.
# It performs no analysis locally and calls no backend during introspection.
# sqlike-mcp is a thin remote forwarder that tokenizes locally at real use time.
FROM node:20-slim
RUN npm install -g @sqlike/mcp
ENTRYPOINT ["sqlike-mcp"]
