#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int32_t StartProxy(
    const char *host,
    int32_t port,
    const char *dc_ips,
    const char *secret,
    int32_t verbose
);
int32_t StopProxy(void);
void SetCfProxyConfig(int32_t enabled, int32_t priority, const char *user_domain);
void SetDpiFragmentation(int32_t v);
void NotifyNetworkChanged(void);
char *GetCfProxyDomains(void);
char *GetStats(void);
void FreeString(char *pointer);

int main(void) {
    const char *secret = "0123456789abcdef0123456789abcdef";
    /* Не 1443: на машине разработчика этот порт уже держит само приложение. */
    const char *port_env = getenv("TGWS_SMOKE_PORT");
    int32_t port = port_env != NULL ? (int32_t)strtol(port_env, NULL, 10) : 34443;

    /* Проверяем поддержку пользовательских доменов (.workers.dev, .ru) и сброс к пулу по умолчанию */
    SetCfProxyConfig(1, 1, "https://my-tg-proxy.example.workers.dev/apiws");
    char *custom_domains = GetCfProxyDomains();
    assert(custom_domains != NULL);
    assert(strstr(custom_domains, "my-tg-proxy.example.workers.dev") != NULL);
    FreeString(custom_domains);

    SetCfProxyConfig(1, 1, "");
    char *default_domains = GetCfProxyDomains();
    assert(default_domains != NULL);
    assert(strstr(default_domains, ".co.uk") != NULL);
    FreeString(default_domains);

    SetDpiFragmentation(1);

    int32_t started = StartProxy("127.0.0.1", port, "", secret, 0);
    if (started != 0) {
        fprintf(stderr, "StartProxy returned %d\n", started);
    }
    assert(started == 0);

    NotifyNetworkChanged();

    char *stats = GetStats();
    assert(stats != NULL);
    assert(strstr(stats, "active=0") != NULL);
    printf("%s\n", stats);
    FreeString(stats);

    assert(StopProxy() == 0);
    return EXIT_SUCCESS;
}
