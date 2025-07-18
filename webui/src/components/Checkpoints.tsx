import React, { useState } from 'react';
import {
  Badge,
  Box,
  Flex,
  Heading,
  Link,
  ListItem,
  Stat,
  StatGroup,
  StatLabel,
  StatNumber,
  Text,
  UnorderedList,
} from '@chakra-ui/react';
import { Checkpoint, Job, Pipeline, useCheckpointDetails } from '../lib/data_fetching';
import { dataFormat } from '../lib/util';
import { CheckpointDetails, formatDuration } from './CheckpointDetails';

export interface CheckpointsProps {
  pipeline: Pipeline;
  job: Job;
  checkpoints: Array<Checkpoint>;
}

const Checkpoints: React.FC<CheckpointsProps> = ({ pipeline, job, checkpoints }) => {
  const [epoch, setEpoch] = useState<number | undefined>(undefined);
  const { checkpointDetails } = useCheckpointDetails(pipeline.id, job.id, epoch);

  if (!checkpoints.length) {
    return <Text textStyle="italic">No checkpoints</Text>;
  }

  let details = <Text>Select checkpoint</Text>;

  let checkpoint: Checkpoint | undefined = checkpoints.find(c => c.epoch == epoch);

  if (checkpoint && checkpointDetails) {
    let start = Number(checkpoint.startTime);
    let end = Number(checkpoint.finishTime ?? new Date().getTime() * 1000);

    let checkpointBytes = checkpointDetails.map(d => d.bytes).reduce((a, b) => a + b, 0);

    const checkpointStats = (
      <StatGroup width={800} border="1px solid #666" borderRadius="5px" marginTop={5} padding={3}>
        <Stat>
          <StatLabel>Started</StatLabel>
          <StatNumber>
            {new Intl.DateTimeFormat('en-us', {
              dateStyle: undefined,
              timeStyle: 'medium',
            }).format(new Date(Number(checkpoint.startTime) / 1000))}
          </StatNumber>
        </Stat>
        <Stat marginLeft={10}>
          <StatLabel>Finished</StatLabel>
          <StatNumber>
            {checkpoint.finishTime != null
              ? new Intl.DateTimeFormat('en-us', {
                  dateStyle: undefined,
                  timeStyle: 'medium',
                }).format(new Date(Number(checkpoint.finishTime) / 1000))
              : '-'}
          </StatNumber>
        </Stat>
        <Stat marginLeft={10}>
          <StatLabel>Duration</StatLabel>
          <StatNumber>{formatDuration(end - start)}</StatNumber>
        </Stat>
        <Stat marginLeft={10}>
          <StatLabel>Total Size</StatLabel>
          <StatNumber> {dataFormat(checkpointBytes)} </StatNumber>
        </Stat>
      </StatGroup>
    );

    details = (
      <Flex flexDirection={'column'} flexGrow={1}>
        <Heading size="md">
          Checkpoint {epoch}
          <Badge marginLeft={2}>{checkpoint.backend}</Badge>
        </Heading>
        {checkpointStats}
        <CheckpointDetails operators={checkpointDetails} checkpoint={checkpoint} />
      </Flex>
    );
  }

  return (
    <Flex>
      <Box w="100px">
        <UnorderedList className="checkpoint-menu">
          {checkpoints
            .slice()
            .reverse()
            .map(c => {
              return (
                <ListItem
                  key={'a' + String(c.epoch)}
                  className={c.epoch == epoch ? 'selected' : ''}
                >
                  <Link onClick={() => setEpoch(c.epoch)}> {c.epoch}</Link>
                </ListItem>
              );
            })}
        </UnorderedList>
      </Box>
      <Flex flexGrow={1}>{details}</Flex>
    </Flex>
  );
};

export default Checkpoints;
