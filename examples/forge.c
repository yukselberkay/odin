/* Forge: small tools, predictable results.
 * Build: cc -std=c11 -Wall -Wextra -Werror examples/forge.c -o target/forge
 * FNV-1a is a non-cryptographic hash, not a security primitive.
 */
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>

static uint32_t fingerprint(const char *text)
{
    uint32_t hash = UINT32_C(2166136261);
    while (*text != '\0') {
        hash ^= (unsigned char)*text++;
        hash *= UINT32_C(16777619);
    }
    return hash;
}

int main(void)
{
    const char *modules[] = {
        "editor", "terminal", "syntax", "git", "ai"
    };
    const size_t count = sizeof modules / sizeof modules[0];

    puts("FORGE / FNV-1a string fingerprints\n");
    for (size_t i = 0; i < count; ++i) {
        printf("  %-10s  %08" PRIx32 "\n",
               modules[i], fingerprint(modules[i]));
    }
    printf("\n%zu fingerprints computed.\n", count);
    return 0;
}
