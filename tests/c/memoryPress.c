// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

/*
This is a very simple test that mallocs lots of space. It should always
pass when the necessary space is available and always fail when it is not
*/
int mem() {
  size_t test = 100;
  void** pointers[10000];
  for (int i = 0; i < 10000; i++) {
    pointers[i] = malloc(test);
  }
  for (int i = 0; i < 10000; i++) {
    free(pointers[i]);
  }
  return 0;
}

int main() {
  assert(mem() == 0);
  return 0;
}
