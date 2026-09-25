// The error a failed api.ts request throws. Its own module because every
// function api.ts exports is taken to be a request (tests/card_api.test.ts).

// A request the server answered with an error status. `detail` is what
// it said: the `error` of a `{"error": …}` body, else the body as text.
export class ApiError extends Error {
  constructor(
    readonly url: string,
    readonly status: number,
    readonly detail: string,
  ) {
    super(detail ? `${url} → ${status}: ${detail}` : `${url} → ${status}`);
  }
}

export function errorDetail(body: string): string {
  const text = body.trim();
  try {
    const parsed: unknown = JSON.parse(text);
    if (parsed && typeof parsed === "object" && "error" in parsed) {
      const error = (parsed as { error: unknown }).error;
      if (typeof error === "string") return error;
    }
  } catch {
    // Not JSON; the text is the detail.
  }
  return text;
}
