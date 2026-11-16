/* doltlite's BLAKE3, SIMD kernels included, against BLAKE3's own test
** vectors (`test_vectors.json`: input byte i is i % 251, `hash` the first
** 131 bytes of output). blake3_hash_oneshot is what doltlite hashes a
** chunk with; the hasher is what it hashes a chunk with a zero tail with.
** The declarations come from the kernels' blake3.h, whose struct and
** version string must be the amalgamation's. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "blake3.h"

size_t blake3_simd_degree(void);
void blake3_hash_oneshot(const void *input, size_t input_len, uint8_t *out,
                         size_t out_len);

static int failures = 0;

static void expect_bytes(const char *what, size_t input_len,
                         const uint8_t *got, const char *hex, size_t n) {
  char buf[2 * BLAKE3_OUT_LEN * 8 + 1];
  for (size_t i = 0; i < n; i++) snprintf(buf + 2 * i, 3, "%02x", got[i]);
  if (strncmp(buf, hex, 2 * n) != 0) {
    fprintf(stderr, "%s of %zu bytes: got %.*s, want %.*s\n", what, input_len,
            (int)(2 * n), buf, (int)(2 * n), hex);
    failures++;
  }
}

static char *read_file(const char *path) {
  FILE *f = fopen(path, "rb");
  if (!f) { perror(path); exit(2); }
  fseek(f, 0, SEEK_END);
  long n = ftell(f);
  fseek(f, 0, SEEK_SET);
  char *s = malloc((size_t)n + 1);
  if (fread(s, 1, (size_t)n, f) != (size_t)n) { perror(path); exit(2); }
  s[n] = 0;
  fclose(f);
  return s;
}

int main(int argc, char **argv) {
  if (argc != 2) { fprintf(stderr, "usage: %s test_vectors.json\n", argv[0]); return 2; }

  if (strcmp(blake3_version(), BLAKE3_VERSION_STRING) != 0) {
    fprintf(stderr, "the amalgamation embeds BLAKE3 %s and the kernels are %s: "
            "move @blake3_c to the embedded version\n",
            blake3_version(), BLAKE3_VERSION_STRING);
    failures++;
  }

  size_t degree = blake3_simd_degree();
  printf("blake3_simd_degree() = %zu\n", degree);
#if defined(__x86_64__) || defined(__aarch64__)
  /* SSE2 is in every x86_64 CPU and NEON in every aarch64 one, and either
  ** hashes 4 chunks at once. 1 is the portable code alone. */
  if (degree < 4) {
    fprintf(stderr, "no SIMD kernel: blake3_simd_degree() is %zu\n", degree);
    failures++;
  }
#endif

  char *json = read_file(argv[1]);
  size_t cases = 0;
  enum { OUT = 131 };
  for (char *p = json; (p = strstr(p, "\"input_len\":")) != NULL; cases++) {
    size_t input_len = strtoul(p + strlen("\"input_len\":"), &p, 10);
    char *h = strstr(p, "\"hash\":");
    if (!h || !(h = strchr(h + strlen("\"hash\":"), '"'))) { fprintf(stderr, "bad json\n"); return 2; }
    const char *hex = h + 1;

    uint8_t *input = malloc(input_len + 1);
    for (size_t i = 0; i < input_len; i++) input[i] = (uint8_t)(i % 251);
    uint8_t out[OUT];

    blake3_hash_oneshot(input, input_len, out, BLAKE3_OUT_LEN);
    expect_bytes("blake3_hash_oneshot", input_len, out, hex, BLAKE3_OUT_LEN);

    blake3_hasher hasher;
    blake3_hasher_init(&hasher);
    blake3_hasher_update(&hasher, input, input_len);
    blake3_hasher_finalize(&hasher, out, OUT);
    expect_bytes("hasher, one update", input_len, out, hex, OUT);

    /* Pieces that straddle chunk (1024) and block (64) boundaries. */
    blake3_hasher_init(&hasher);
    for (size_t off = 0, step = 1; off < input_len; off += step, step = step * 3 + 1) {
      size_t n = input_len - off < step ? input_len - off : step;
      blake3_hasher_update(&hasher, input + off, n);
    }
    blake3_hasher_finalize(&hasher, out, OUT);
    expect_bytes("hasher, in pieces", input_len, out, hex, OUT);

    free(input);
    p = (char *)hex;
  }
  free(json);

  if (cases < 30) { fprintf(stderr, "read only %zu cases\n", cases); failures++; }
  printf("%zu cases, %d failures\n", cases, failures);
  return failures == 0 ? 0 : 1;
}
