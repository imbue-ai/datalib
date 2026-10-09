/**
 * The plugin's entry point: what Latchkey imports from `~/.latchkey/plugins/<name>/`.
 */
import { createGarmin } from './garmin.js';
export default function plugin(sdk) {
    const { Garmin, GarminCredentials } = createGarmin(sdk);
    return {
        latchkeyVersion: '^3.15.0',
        services: [new Garmin()],
        apiCredentialsTypes: [GarminCredentials],
    };
}
