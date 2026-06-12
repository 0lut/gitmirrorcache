const API_PATHS = ["/healthz", "/metrics"];
const API_PREFIXES = ["/git/", "/v1/"];

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (isApiRequest(url.pathname)) {
      return proxyToApi(request, env.API_ORIGIN);
    }

    return env.ASSETS.fetch(request);
  },
};

function isApiRequest(pathname) {
  return API_PATHS.includes(pathname) || API_PREFIXES.some((prefix) => pathname.startsWith(prefix));
}

async function proxyToApi(request, apiOrigin) {
  if (!apiOrigin) {
    return new Response("API origin is not configured\n", { status: 503 });
  }

  const incomingUrl = new URL(request.url);
  const targetUrl = new URL(apiOrigin);
  targetUrl.pathname = incomingUrl.pathname;
  targetUrl.search = incomingUrl.search;

  const headers = new Headers(request.headers);
  headers.delete("host");
  headers.set("x-forwarded-host", incomingUrl.host);
  headers.set("x-forwarded-proto", incomingUrl.protocol.slice(0, -1));

  const init = {
    method: request.method,
    headers,
    redirect: "manual",
  };
  if (request.method !== "GET" && request.method !== "HEAD") {
    init.body = request.body;
  }

  return fetch(new Request(targetUrl, init));
}
