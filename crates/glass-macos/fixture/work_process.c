#include <stdio.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void leaf(void) {
    while (getppid() != 1) usleep(1000);
    printf("%d\n", getpid());
    fflush(stdout);
    char byte;
    (void)read(STDIN_FILENO, &byte, 1);
    _exit(0);
}

int main(int argc, char **argv) {
    if (argc != 2) return 64;
    if (strcmp(argv[1], "exec-leaf")) {
        char release;
        if (read(STDIN_FILENO, &release, 1) != 1) return 0;
    }
    if (!strcmp(argv[1], "exec")) {
        puts("ready");
        fflush(stdout);
        char byte;
        if (read(STDIN_FILENO, &byte, 1) != 1) return 1;
        execl(argv[0], argv[0], "exec-leaf", NULL);
        return 2;
    }
    if (!strcmp(argv[1], "exec-leaf")) {
        puts("executed");
        fflush(stdout);
        char byte;
        (void)read(STDIN_FILENO, &byte, 1);
        return 0;
    }
    int child = fork();
    if (child < 0) return 3;
    if (child > 0) return 0;
    if (setsid() < 0) _exit(4);
    if (!strcmp(argv[1], "double")) {
        child = fork();
        if (child < 0) _exit(5);
        if (child > 0) _exit(0);
    } else if (!strcmp(argv[1], "resist")) {
        signal(SIGTERM, SIG_IGN);
    } else if (strcmp(argv[1], "single")) _exit(64);
    leaf();
}
