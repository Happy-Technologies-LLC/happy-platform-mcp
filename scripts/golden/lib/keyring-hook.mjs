// Module-resolution hook: redirect @napi-rs/keyring to the fake keyring.
export async function resolve(specifier, context, nextResolve) {
  if (specifier === '@napi-rs/keyring') {
    return { url: new URL('./fake-keyring.mjs', import.meta.url).href, shortCircuit: true };
  }
  return nextResolve(specifier, context);
}
