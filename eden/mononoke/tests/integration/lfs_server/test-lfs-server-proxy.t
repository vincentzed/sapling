# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

# Create two repositories
  $ setup_common_config blob_files
  $ REPOID=1 FILESTORE=1 FILESTORE_CHUNK_SIZE=10 LFS_USE_UPSTREAM=1 setup_mononoke_repo_config lfs_proxy
  $ REPOID=2 FILESTORE=1 FILESTORE_CHUNK_SIZE=10 setup_mononoke_repo_config lfs_upstream

# Start a LFS server (lfs_upstream is an upstream of lfs_proxy)
  $ log_proxy="$TESTTMP/lfs_proxy.log"
  $ log_upstream="$TESTTMP/lfs_upstream.log"
  $ SCUBA="$TESTTMP/scuba.json"

  $ lfs_upstream="$(lfs_server --log "$log_upstream" --scuba-log-file "$SCUBA")/lfs_upstream"
  $ lfs_proxy="$(lfs_server --always-wait-for-upstream --upstream "$lfs_upstream" --log "$log_proxy")/lfs_proxy"

# Upload data to upstream only
  $ yes A 2>/dev/null | head -c 2KiB | hg debuglfssend "$lfs_upstream"
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048

  $ cat "$log_proxy"

  $ cat "$log_upstream"
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > PUT /lfs_upstream/upload/ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746/2048?server_hostname=* - (glob)
  OUT < PUT /lfs_upstream/upload/ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746/2048?server_hostname=* 200 OK (glob)

  $ truncate -s 0 "$log_proxy" "$log_upstream"

# Reading data should succeed if it is in upstream
  $ hg debuglfsreceive ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048 "$lfs_proxy" | sha256sum
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746  -

  $ cat "$log_proxy"
  IN  > POST /lfs_proxy/objects/batch -
  OUT < POST /lfs_proxy/objects/batch 200 OK

  $ cat "$log_upstream"
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > GET /lfs_upstream/download/d28548bc21aabf04d143886d717d72375e3deecd0dafb3d110676b70a192cb5d?server_hostname=* - (glob)
  OUT < GET /lfs_upstream/download/d28548bc21aabf04d143886d717d72375e3deecd0dafb3d110676b70a192cb5d?server_hostname=* 2* (glob)

  $ truncate -s 0 "$log_proxy" "$log_upstream"

# Uploading data that is present in upstream but not locally should trigger a new upload
  $ yes A 2>/dev/null | head -c 2KiB | hg debuglfssend "$lfs_proxy"
  ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746 2048

  $ cat "$log_proxy"
  IN  > POST /lfs_proxy/objects/batch -
  OUT < POST /lfs_proxy/objects/batch 200 OK
  IN  > PUT /lfs_proxy/upload/ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746/2048?server_hostname=* - (glob)
  OUT < PUT /lfs_proxy/upload/ab02c2a1923c8eb11cb3ddab70320746d71d32ad63f255698dc67c3295757746/2048?server_hostname=* 200 OK (glob)

  $ cat "$log_upstream"
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK

  $ wait_for_json_record_count "$SCUBA" 6
  $ truncate -s 0 "$log_proxy" "$log_upstream" "$SCUBA"

# Uploading should make data available in both locations
  $ yes B 2>/dev/null | head -c 2KiB | hg debuglfssend "$lfs_proxy"
  a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84 2048


  $ cat "$log_proxy"
  IN  > POST /lfs_proxy/objects/batch -
  OUT < POST /lfs_proxy/objects/batch 200 OK
  IN  > PUT /lfs_proxy/upload/a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84/2048?server_hostname=* - (glob)
  OUT < PUT /lfs_proxy/upload/a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84/2048?server_hostname=* 200 OK (glob)

  $ cat "$log_upstream"
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > PUT /lfs_upstream/upload/a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84/2048?server_hostname=* - (glob)
  OUT < PUT /lfs_upstream/upload/a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84/2048?server_hostname=* 200 OK (glob)

# Proper user agent should be sent to proxy.
  $ wait_for_json_record_count "$SCUBA" 3
  $ format_single_scuba_sample < "$SCUBA" | grep agent
      "http_user_agent": "mononoke-lfs-server/0.1.0 git/2.15.1",
      "http_user_agent": "mononoke-lfs-server/0.1.0 git/2.15.1",
      "http_user_agent": "mononoke-lfs-server/0.1.0 git/2.15.1",
  $ truncate -s 0 "$log_proxy" "$log_upstream"

  $ hg debuglfsreceive a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84 2048 "$lfs_proxy" | sha256sum
  a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84  -

  $ hg debuglfsreceive a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84 2048 "$lfs_upstream" | sha256sum
  a1bcf2c963bec9588aaa30bd33ef07873792e3ec241453b0d21635d1c4bbae84  -

  $ cat "$log_proxy"
  IN  > POST /lfs_proxy/objects/batch -
  OUT < POST /lfs_proxy/objects/batch 200 OK
  IN  > GET /lfs_proxy/download/2e8e6e2dda2bb7b6458146a1c1bf301e4856293e1cc258ab789c63df2254693c?server_hostname=* - (glob)
  OUT < GET /lfs_proxy/download/2e8e6e2dda2bb7b6458146a1c1bf301e4856293e1cc258ab789c63df2254693c?server_hostname=* 2* (glob)

  $ cat "$log_upstream"
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > POST /lfs_upstream/objects/batch -
  OUT < POST /lfs_upstream/objects/batch 200 OK
  IN  > GET /lfs_upstream/download/2e8e6e2dda2bb7b6458146a1c1bf301e4856293e1cc258ab789c63df2254693c?server_hostname=* - (glob)
  OUT < GET /lfs_upstream/download/2e8e6e2dda2bb7b6458146a1c1bf301e4856293e1cc258ab789c63df2254693c?server_hostname=* 2* (glob)

  $ truncate -s 0 "$log_proxy" "$log_upstream"
