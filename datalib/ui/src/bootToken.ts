// Imported first by main.ts: the router reads the address bar when its
// module loads, and the launch URL's `?token=` must be gone by then.
import { adoptLaunchToken, installTokenOnFetch } from "./apiToken";

adoptLaunchToken();
installTokenOnFetch();
