#!/usr/bin/env bash
# Provision an Ubuntu VM. Run deploy/start.sh on it after installing Docker.
set -euo pipefail
: "${AWS_REGION:?Set AWS_REGION}"
: "${SUBNET_ID:?Set SUBNET_ID}"
: "${SECURITY_GROUP_ID:?Set a security group allowing 80/443 and restricted SSH}"
: "${KEY_NAME:?Set an existing EC2 SSH key-pair name}"
: "${AMI_ID:?Set an Ubuntu 24.04 amd64 AMI ID for this region}"
aws ec2 run-instances --region "$AWS_REGION" --image-id "$AMI_ID" \
  --instance-type "${INSTANCE_TYPE:-t3.small}" --key-name "$KEY_NAME" \
  --subnet-id "$SUBNET_ID" --security-group-ids "$SECURITY_GROUP_ID" \
  --associate-public-ip-address --metadata-options HttpTokens=required \
  --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3","Encrypted":true,"DeleteOnTermination":false}}]' \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Name,Value=pine}]' \
  --query 'Instances[0].InstanceId' --output text
