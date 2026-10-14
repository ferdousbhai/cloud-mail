// API_TOKEN is a Worker secret. `cloudmail setup` sets it with
// `cf workers secrets update` after the first deploy. cloudflare.config.ts
// leaves it undeclared so `cf deploy` keeps the secret already on the Worker,
// and `cf workers types` therefore omits it from the generated Env.
interface Env {
  API_TOKEN: string;
}
