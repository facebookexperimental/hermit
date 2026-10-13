#include <stdio.h>

static int square(int x) {
  int y = x * x;
  return y;
}

int main(void) {
  int total = 0;
  for (int i = 0; i < 3; i++) {
    total += square(i);
  }
  printf("total=%d\n", total);
  return 0;
}
