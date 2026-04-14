// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/falloc.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../../common/test.h"

#define BASE_DIR "/ext2/fallocate_test"

#ifndef FALLOC_FL_KEEP_SIZE
#define FALLOC_FL_KEEP_SIZE 0x01
#endif

#ifndef FALLOC_FL_PUNCH_HOLE
#define FALLOC_FL_PUNCH_HOLE 0x02
#endif

#ifndef FALLOC_FL_COLLAPSE_RANGE
#define FALLOC_FL_COLLAPSE_RANGE 0x08
#endif

FN_SETUP(prepare_base_dir)
{
	CHECK_WITH(mkdir(BASE_DIR, 0755), _ret == 0 || errno == EEXIST);
}
END_SETUP()

FN_TEST(fallocate_extends_size)
{
	const char *path = BASE_DIR "/extends_size";
	struct stat st;

	int fd = TEST_SUCC(open(path, O_CREAT | O_RDWR, 0644));
	TEST_SUCC(fallocate(fd, 0, 0, 4096));
	TEST_SUCC(fstat(fd, &st));
	TEST_RES(fstat(fd, &st), st.st_size == 4096);
	TEST_SUCC(close(fd));
	TEST_SUCC(unlink(path));
}
END_TEST()

FN_TEST(fallocate_bad_mode_eopnotsupp)
{
	const char *path = BASE_DIR "/bad_mode";

	int fd = TEST_SUCC(open(path, O_CREAT | O_RDWR, 0644));
	TEST_ERRNO(fallocate(fd, FALLOC_FL_COLLAPSE_RANGE, 0, 4096),
		   EOPNOTSUPP);
	TEST_SUCC(close(fd));
	TEST_SUCC(unlink(path));
}
END_TEST()

FN_TEST(fallocate_keep_size)
{
	const char *path = BASE_DIR "/keep_size";
	struct stat st;
	char buf[] = "hello";

	int fd = TEST_SUCC(open(path, O_CREAT | O_RDWR, 0644));

	errno = 0;
	int rc = fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 8192);
	SKIP_TEST_IF(rc == -1 && errno == EOPNOTSUPP);

	TEST_RES(fstat(fd, &st), st.st_size == 0);
	TEST_RES(pwrite(fd, buf, sizeof(buf), 4096), _ret == sizeof(buf));
	TEST_SUCC(close(fd));
	TEST_SUCC(unlink(path));
}
END_TEST()

FN_TEST(fallocate_punch_hole)
{
	const char *path = BASE_DIR "/punch_hole";
	char write_buf[8192];
	char read_buf[8192];

	memset(write_buf, 'A', sizeof(write_buf));

	int fd = TEST_SUCC(open(path, O_CREAT | O_RDWR, 0644));
	TEST_RES(write(fd, write_buf, sizeof(write_buf)),
		 _ret == sizeof(write_buf));

	errno = 0;
	int rc = fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0,
			   4096);
	SKIP_TEST_IF(rc == -1 && errno == EOPNOTSUPP);

	TEST_RES(pread(fd, read_buf, 4096, 0), _ret == 4096);

	int first_ok = 1;
	for (int i = 0; i < 4096; i++) {
		if (read_buf[i] != '\0') {
			first_ok = 0;
			break;
		}
	}
	TEST_RES(pread(fd, read_buf, 4096, 0), first_ok);

	TEST_RES(pread(fd, read_buf, 4096, 4096), _ret == 4096);

	int second_ok = 1;
	for (int i = 0; i < 4096; i++) {
		if (read_buf[i] != 'A') {
			second_ok = 0;
			break;
		}
	}
	TEST_RES(pread(fd, read_buf, 4096, 4096), second_ok);

	TEST_SUCC(close(fd));
	TEST_SUCC(unlink(path));
}
END_TEST()
