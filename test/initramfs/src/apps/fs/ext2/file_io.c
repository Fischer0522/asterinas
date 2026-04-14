// SPDX-License-Identifier: MPL-2.0

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include "../../common/test.h"

#define BASE_DIR "/ext2/file_io_test"

static void ensure_dir(const char *path)
{
	CHECK_WITH(mkdir(path, 0755), _ret >= 0 || errno == EEXIST);
}

static void remove_dir_if_exists(const char *path)
{
	CHECK_WITH(rmdir(path), _ret == 0 || errno == ENOENT);
}

static void unlink_if_exists(const char *path)
{
	CHECK_WITH(unlink(path), _ret == 0 || errno == ENOENT);
}

FN_SETUP(create_base_dir)
{
	ensure_dir(BASE_DIR);
}
END_SETUP()

FN_TEST(write_close_reopen_read)
{
	const char *path = BASE_DIR "/test_write_read";
	const char *msg = "hello world";
	char buf[64] = { 0 };

	int fd = TEST_SUCC(open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644));
	TEST_RES(write(fd, msg, strlen(msg)), _ret == (ssize_t)strlen(msg));
	TEST_SUCC(close(fd));

	fd = TEST_SUCC(open(path, O_RDONLY));
	TEST_RES(read(fd, buf, sizeof(buf)), _ret == (ssize_t)strlen(msg));
	TEST_RES(memcmp(buf, msg, strlen(msg)), _ret == 0);
	TEST_SUCC(close(fd));

	unlink_if_exists(path);
}
END_TEST()

FN_TEST(write_partial_block)
{
	const char *path = BASE_DIR "/test_partial_block";
	char wbuf[100];
	char rbuf[100];

	memset(wbuf, 'A', sizeof(wbuf));

	int fd = TEST_SUCC(open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644));
	TEST_RES(write(fd, wbuf, sizeof(wbuf)), _ret == sizeof(wbuf));
	TEST_SUCC(close(fd));

	fd = TEST_SUCC(open(path, O_RDONLY));
	TEST_RES(read(fd, rbuf, sizeof(rbuf)), _ret == sizeof(rbuf));
	TEST_RES(memcmp(rbuf, wbuf, sizeof(wbuf)), _ret == 0);
	TEST_SUCC(close(fd));

	unlink_if_exists(path);
}
END_TEST()

FN_TEST(write_cross_block)
{
	const char *path = BASE_DIR "/test_cross_block";
	char wbuf[8192];
	char rbuf[8192];

	for (int i = 0; i < (int)sizeof(wbuf); i++)
		wbuf[i] = (char)(i & 0xff);

	int fd = TEST_SUCC(open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644));
	TEST_RES(write(fd, wbuf, sizeof(wbuf)), _ret == sizeof(wbuf));
	TEST_SUCC(close(fd));

	fd = TEST_SUCC(open(path, O_RDONLY));
	TEST_RES(read(fd, rbuf, sizeof(rbuf)), _ret == sizeof(rbuf));
	TEST_RES(memcmp(rbuf, wbuf, sizeof(wbuf)), _ret == 0);
	TEST_SUCC(close(fd));

	unlink_if_exists(path);
}
END_TEST()

FN_TEST(write_dir_fd_eisdir)
{
	const char *dir_path = BASE_DIR "/test_dir_write";

	ensure_dir(dir_path);

	int fd = TEST_SUCC(open(dir_path, O_RDONLY | O_DIRECTORY));
	TEST_ERRNO(write(fd, "x", 1), EISDIR);
	TEST_SUCC(close(fd));

	remove_dir_if_exists(dir_path);
}
END_TEST()

FN_TEST(read_dir_fd_eisdir)
{
	const char *dir_path = BASE_DIR "/test_dir_read";
	char buf[1];

	ensure_dir(dir_path);

	int fd = TEST_SUCC(open(dir_path, O_RDONLY | O_DIRECTORY));
	TEST_ERRNO(read(fd, buf, sizeof(buf)), EISDIR);
	TEST_SUCC(close(fd));

	remove_dir_if_exists(dir_path);
}
END_TEST()

FN_TEST(write_append)
{
	const char *path = BASE_DIR "/test_append";
	const char *first = "first";
	const char *second = "second";
	const char *expected = "firstsecond";
	char buf[64] = { 0 };

	int fd = TEST_SUCC(
		open(path, O_CREAT | O_WRONLY | O_TRUNC | O_APPEND, 0644));
	TEST_RES(write(fd, first, strlen(first)),
		 _ret == (ssize_t)strlen(first));
	TEST_RES(write(fd, second, strlen(second)),
		 _ret == (ssize_t)strlen(second));
	TEST_SUCC(close(fd));

	fd = TEST_SUCC(open(path, O_RDONLY));
	TEST_RES(read(fd, buf, sizeof(buf)), _ret == (ssize_t)strlen(expected));
	TEST_RES(memcmp(buf, expected, strlen(expected)), _ret == 0);
	TEST_SUCC(close(fd));

	unlink_if_exists(path);
}
END_TEST()

FN_TEST(pwrite_pread)
{
	const char *path = BASE_DIR "/test_pwrite_pread";
	const char *data = "ABCD";
	char buf[4] = { 0 };
	char zero_buf[4] = { 0 };

	int fd = TEST_SUCC(open(path, O_CREAT | O_RDWR | O_TRUNC, 0644));
	TEST_RES(pwrite(fd, data, 4, 100), _ret == 4);

	TEST_RES(pread(fd, buf, 4, 100), _ret == 4);
	TEST_RES(memcmp(buf, data, 4), _ret == 0);

	// Bytes before the pwrite offset should be zero
	memset(buf, 0xff, sizeof(buf));
	TEST_RES(pread(fd, buf, 4, 0), _ret == 4);
	TEST_RES(memcmp(buf, zero_buf, 4), _ret == 0);

	TEST_SUCC(close(fd));

	unlink_if_exists(path);
}
END_TEST()
