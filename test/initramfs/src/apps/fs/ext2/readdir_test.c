// SPDX-License-Identifier: MPL-2.0

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../../common/test.h"

#define BASE_DIR "/ext2/readdir_test"

static void ensure_dir(const char *path)
{
	CHECK_WITH(mkdir(path, 0755), _ret >= 0 || errno == EEXIST);
}

static void remove_if_exists(const char *path)
{
	CHECK_WITH(rmdir(path), _ret == 0 || errno == ENOENT);
}

static void unlink_if_exists(const char *path)
{
	CHECK_WITH(unlink(path), _ret == 0 || errno == ENOENT);
}

FN_SETUP(setup_base_dir)
{
	ensure_dir(BASE_DIR);
}
END_SETUP()

FN_TEST(readdir_returns_all_entries)
{
	const char *dir = BASE_DIR "/all_entries";
	const char *names[] = { "a", "b", "c", "d", "e" };
	const int num_files = 5;
	char path[256];
	int found[7] = { 0 }; // 5 files + "." + ".."

	ensure_dir(dir);
	for (int i = 0; i < num_files; i++) {
		snprintf(path, sizeof(path), "%s/%s", dir, names[i]);
		int fd = CHECK(open(path, O_CREAT | O_WRONLY, 0644));
		CHECK(close(fd));
	}

	DIR *dp = opendir(dir);
	CHECK_WITH(dp == NULL ? -1 : 0, _ret >= 0);

	struct dirent *ent;
	int count = 0;
	while ((ent = readdir(dp)) != NULL) {
		if (strcmp(ent->d_name, ".") == 0)
			found[5] = 1;
		else if (strcmp(ent->d_name, "..") == 0)
			found[6] = 1;
		else {
			for (int i = 0; i < num_files; i++) {
				if (strcmp(ent->d_name, names[i]) == 0)
					found[i] = 1;
			}
		}
		count++;
	}
	closedir(dp);

	if (count != 7) {
		__tests_failed++;
		fprintf(stderr,
			"%s: readdir count: expected 7, got %d\n",
			__func__, count);
	} else {
		__tests_passed++;
	}
	for (int i = 0; i < 7; i++) {
		if (!found[i]) {
			__tests_failed++;
			fprintf(stderr,
				"%s: entry %d not found\n",
				__func__, i);
		} else {
			__tests_passed++;
		}
	}

	// Cleanup
	for (int i = num_files - 1; i >= 0; i--) {
		snprintf(path, sizeof(path), "%s/%s", dir, names[i]);
		unlink_if_exists(path);
	}
	remove_if_exists(dir);
}
END_TEST()

FN_TEST(readdir_seekdir_resume)
{
	const char *dir = BASE_DIR "/seekdir_test";
	char path[256];
	const int num_files = 6;

	ensure_dir(dir);
	for (int i = 0; i < num_files; i++) {
		snprintf(path, sizeof(path), "%s/file_%03d", dir, i);
		int fd = CHECK(open(path, O_CREAT | O_WRONLY, 0644));
		CHECK(close(fd));
	}

	// First pass: read a few entries and record telldir position
	DIR *dp = opendir(dir);
	CHECK_WITH(dp == NULL ? -1 : 0, _ret >= 0);

	struct dirent *ent;
	int skip = 3;
	for (int i = 0; i < skip; i++)
		ent = readdir(dp);

	long saved_pos = telldir(dp);

	// Collect remaining entries from saved position
	char first_remaining[256] = { 0 };
	ent = readdir(dp);
	if (ent)
		snprintf(first_remaining, sizeof(first_remaining), "%s", ent->d_name);
	closedir(dp);

	// Second pass: reopen, seekdir, verify same entry appears
	dp = opendir(dir);
	CHECK_WITH(dp == NULL ? -1 : 0, _ret >= 0);

	seekdir(dp, saved_pos);
	ent = readdir(dp);
	if (ent)
		TEST_RES(strcmp(ent->d_name, first_remaining),
			 _ret == 0);
	closedir(dp);

	// Cleanup
	for (int i = num_files - 1; i >= 0; i--) {
		snprintf(path, sizeof(path), "%s/file_%03d", dir, i);
		unlink_if_exists(path);
	}
	remove_if_exists(dir);
}
END_TEST()

FN_TEST(mkdir_contains_dot_dotdot)
{
	const char *dir = BASE_DIR "/dot_dotdot";

	ensure_dir(dir);

	DIR *dp = opendir(dir);
	CHECK_WITH(dp == NULL ? -1 : 0, _ret >= 0);

	struct dirent *ent;
	int count = 0;
	int found_dot = 0, found_dotdot = 0;
	while ((ent = readdir(dp)) != NULL) {
		if (strcmp(ent->d_name, ".") == 0)
			found_dot = 1;
		else if (strcmp(ent->d_name, "..") == 0)
			found_dotdot = 1;
		count++;
	}
	closedir(dp);

	if (count != 2) {
		__tests_failed++;
		fprintf(stderr,
			"%s: expected 2 entries, got %d\n",
			__func__, count);
	} else {
		__tests_passed++;
	}
	if (!found_dot) {
		__tests_failed++;
		fprintf(stderr, "%s: '.' not found\n", __func__);
	} else {
		__tests_passed++;
	}
	if (!found_dotdot) {
		__tests_failed++;
		fprintf(stderr, "%s: '..' not found\n", __func__);
	} else {
		__tests_passed++;
	}

	remove_if_exists(dir);
}
END_TEST()

FN_TEST(rmdir_then_stat_enoent)
{
	const char *dir = BASE_DIR "/rmdir_stat";
	struct stat st;

	ensure_dir(dir);
	CHECK(rmdir(dir));

	TEST_ERRNO(stat(dir, &st), ENOENT);
}
END_TEST()

FN_TEST(dir_growth_many_files)
{
	const char *dir = BASE_DIR "/many_files";
	const int num_files = 200;
	char path[256];

	ensure_dir(dir);

	for (int i = 0; i < num_files; i++) {
		snprintf(path, sizeof(path), "%s/file_%03d", dir, i);
		int fd = CHECK(open(path, O_CREAT | O_WRONLY, 0644));
		CHECK(close(fd));
	}

	DIR *dp = opendir(dir);
	CHECK_WITH(dp == NULL ? -1 : 0, _ret >= 0);

	int count = 0;
	while (readdir(dp) != NULL)
		count++;
	closedir(dp);

	if (count != num_files + 2) {
		__tests_failed++;
		fprintf(stderr,
			"%s: expected %d entries, got %d\n",
			__func__, num_files + 2, count);
	} else {
		__tests_passed++;
	}

	// Cleanup in reverse order
	for (int i = num_files - 1; i >= 0; i--) {
		snprintf(path, sizeof(path), "%s/file_%03d", dir, i);
		unlink_if_exists(path);
	}
	remove_if_exists(dir);
}
END_TEST()

FN_TEST(dot_dotdot_semantics)
{
	const char *parent = BASE_DIR "/parent";
	const char *child = BASE_DIR "/parent/child";
	struct stat st_parent, st_child, st_dot, st_dotdot;
	char dot_path[256], dotdot_path[256];

	ensure_dir(parent);
	ensure_dir(child);

	CHECK(stat(parent, &st_parent));
	CHECK(stat(child, &st_child));

	snprintf(dot_path, sizeof(dot_path), "%s/.", child);
	snprintf(dotdot_path, sizeof(dotdot_path), "%s/..", child);

	CHECK(stat(dot_path, &st_dot));
	CHECK(stat(dotdot_path, &st_dotdot));

	if ((ino_t)st_dot.st_ino != (ino_t)st_child.st_ino) {
		__tests_failed++;
		fprintf(stderr,
			"%s: child/. ino mismatch\n", __func__);
	} else {
		__tests_passed++;
	}
	if ((ino_t)st_dotdot.st_ino != (ino_t)st_parent.st_ino) {
		__tests_failed++;
		fprintf(stderr,
			"%s: child/.. ino mismatch\n", __func__);
	} else {
		__tests_passed++;
	}

	remove_if_exists(child);
	remove_if_exists(parent);
}
END_TEST()
