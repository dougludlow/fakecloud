<?php

declare(strict_types=1);

namespace FakeCloud\Tests;

use FakeCloud\FakeCloud;
use FakeCloud\FakeCloudError;
use FakeCloud\PutServiceQuotaEnforcementRequest;
use FakeCloud\PutServiceQuotaRequest;
use FakeCloud\QuotaEnforcement;
use FakeCloud\ServiceQuotaEnforcementChange;
use FakeCloud\ServiceQuotasClient;
use PHPUnit\Framework\TestCase;
use RuntimeException;

/**
 * Unit tests for the Service Quotas sub-client against a mocked HTTP API:
 * a `php -S` server running tests/fixtures/mock-server.php records each
 * request and answers with a canned response.
 */
final class ServiceQuotasClientTest extends TestCase
{
    /** @var resource|null */
    private static $process = null;
    private static string $dir = '';
    private static string $endpoint = '';

    private ServiceQuotasClient $sq;

    public static function setUpBeforeClass(): void
    {
        self::$dir = sys_get_temp_dir() . '/fakecloud-php-mock-' . bin2hex(random_bytes(6));
        if (!mkdir(self::$dir) && !is_dir(self::$dir)) {
            throw new RuntimeException('could not create ' . self::$dir);
        }

        $sock = stream_socket_server('tcp://127.0.0.1:0');
        if ($sock === false) {
            throw new RuntimeException('could not allocate a port');
        }
        $name = (string) stream_socket_get_name($sock, false);
        fclose($sock);
        $port = (int) substr($name, strrpos($name, ':') + 1);
        self::$endpoint = 'http://127.0.0.1:' . $port;

        $env = getenv();
        $env['MOCK_DIR'] = self::$dir;
        $pipes = [];
        $proc = proc_open(
            [PHP_BINARY, '-S', '127.0.0.1:' . $port, __DIR__ . '/fixtures/mock-server.php'],
            [0 => ['file', '/dev/null', 'r'], 1 => ['file', '/dev/null', 'w'], 2 => ['file', '/dev/null', 'w']],
            $pipes,
            null,
            $env,
        );
        if (!is_resource($proc)) {
            throw new RuntimeException('could not start the mock server');
        }
        self::$process = $proc;

        $deadline = microtime(true) + 10.0;
        while (microtime(true) < $deadline) {
            $conn = @fsockopen('127.0.0.1', $port, $errno, $errstr, 0.2);
            if ($conn !== false) {
                fclose($conn);
                return;
            }
            usleep(50_000);
        }
        throw new RuntimeException('mock server never became ready on port ' . $port);
    }

    public static function tearDownAfterClass(): void
    {
        if (self::$process !== null) {
            proc_terminate(self::$process);
            proc_close(self::$process);
            self::$process = null;
        }
        foreach (glob(self::$dir . '/*') ?: [] as $f) {
            unlink($f);
        }
        @rmdir(self::$dir);
    }

    protected function setUp(): void
    {
        $this->sq = (new FakeCloud(self::$endpoint))->serviceQuotas();
        @unlink(self::$dir . '/request.json');
    }

    private function respond(int $status, mixed $body): void
    {
        file_put_contents(self::$dir . '/response.json', json_encode([
            'status' => $status,
            'body' => json_encode($body),
        ]));
    }

    /** @return array{method: string, uri: string, body: string} */
    private function lastRequest(): array
    {
        return json_decode((string) file_get_contents(self::$dir . '/request.json'), true);
    }

    /** The decoded JSON body of the last request. */
    private function lastBody(): mixed
    {
        return json_decode($this->lastRequest()['body'], true);
    }

    private static function quota(array $overrides = []): array
    {
        return array_merge([
            'serviceCode' => 'ec2',
            'quotaCode' => 'L-0263D0A3',
            'quotaName' => 'EC2-VPC Elastic IPs',
            'global' => false,
            'adjustable' => true,
            'unit' => 'None',
            'defaultValue' => 5.0,
            'appliedValue' => 2.0,
            'usage' => 1.0,
            'enforceable' => true,
            'enforced' => true,
            'enforcementSource' => 'override',
        ], $overrides);
    }

    private static function request(array $overrides = []): array
    {
        return array_merge([
            'accountId' => '123456789012',
            'requestId' => 'req-1',
            'serviceCode' => 'ec2',
            'quotaCode' => 'L-0263D0A3',
            'quotaName' => 'EC2-VPC Elastic IPs',
            'region' => 'us-east-1',
            'desiredValue' => 10.0,
            'status' => 'APPROVED',
            'caseId' => null,
            'created' => '2026-10-05T12:00:00+00:00',
            'lastUpdated' => '2026-10-05T12:01:00+00:00',
        ], $overrides);
    }

    public function testAccessor(): void
    {
        $this->assertInstanceOf(ServiceQuotasClient::class, (new FakeCloud())->serviceQuotas());
    }

    public function testGetQuotasWithoutFilters(): void
    {
        $this->respond(200, ['accountId' => '123456789012', 'region' => 'us-east-1', 'quotas' => []]);
        $resp = $this->sq->getQuotas();
        $req = $this->lastRequest();
        $this->assertSame('GET', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/quotas', $req['uri']);
        $this->assertSame([], $resp->quotas);
    }

    public function testGetQuotasWithQueryParams(): void
    {
        $this->respond(200, [
            'accountId' => '111122223333',
            'region' => 'eu-west-1',
            'quotas' => [self::quota(), self::quota(['quotaCode' => 'L-X', 'usage' => null, 'enforceable' => false, 'enforced' => false, 'enforcementSource' => 'not_enforceable'])],
        ]);
        $resp = $this->sq->getQuotas(accountId: '111122223333', region: 'eu-west-1', serviceCode: 'ec2');
        $this->assertSame(
            '/_fakecloud/service-quotas/quotas?accountId=111122223333&region=eu-west-1&serviceCode=ec2',
            $this->lastRequest()['uri'],
        );
        $this->assertSame('111122223333', $resp->accountId);
        $this->assertSame('eu-west-1', $resp->region);
        $this->assertCount(2, $resp->quotas);
        $q = $resp->quotas[0];
        $this->assertSame('ec2', $q->serviceCode);
        $this->assertSame('L-0263D0A3', $q->quotaCode);
        $this->assertSame('EC2-VPC Elastic IPs', $q->quotaName);
        $this->assertFalse($q->global);
        $this->assertTrue($q->adjustable);
        $this->assertSame('None', $q->unit);
        $this->assertSame(5.0, $q->defaultValue);
        $this->assertSame(2.0, $q->appliedValue);
        $this->assertSame(1.0, $q->usage);
        $this->assertTrue($q->enforced);
        $this->assertSame('override', $q->enforcementSource);
        $this->assertNull($resp->quotas[1]->usage);
        $this->assertSame('not_enforceable', $resp->quotas[1]->enforcementSource);
    }

    public function testPutQuotaOmitsEnforceWhenUnset(): void
    {
        $this->respond(200, self::quota());
        $q = $this->sq->putQuota('ec2', 'L-0263D0A3', new PutServiceQuotaRequest(value: 2.0));
        $req = $this->lastRequest();
        $this->assertSame('PUT', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/quotas/ec2/L-0263D0A3', $req['uri']);
        $body = $this->lastBody();
        // json_encode drops the zero fraction; the server reads 2 as 2.0.
        $this->assertSame(['value' => 2], $body);
        $this->assertArrayNotHasKey('enforce', $body);
        $this->assertSame(2.0, $q->appliedValue);
    }

    public function testPutQuotaSendsExplicitNullForDefault(): void
    {
        $this->respond(200, self::quota());
        $this->sq->putQuota('ec2', 'L-0263D0A3', new PutServiceQuotaRequest(enforcement: QuotaEnforcement::Default));
        $body = $this->lastBody();
        $this->assertArrayHasKey('enforce', $body);
        $this->assertNull($body['enforce']);
        $this->assertStringContainsString('"enforce":null', $this->lastRequest()['body']);
    }

    public function testPutQuotaSendsEnforceTrueAndFalse(): void
    {
        $this->respond(200, self::quota());
        $this->sq->putQuota('ec2', 'L-0263D0A3', new PutServiceQuotaRequest(
            accountId: '111122223333',
            region: 'eu-west-1',
            value: 3.5,
            enforcement: QuotaEnforcement::Enforce,
        ));
        $this->assertSame(
            ['accountId' => '111122223333', 'region' => 'eu-west-1', 'value' => 3.5, 'enforce' => true],
            $this->lastBody(),
        );

        $this->sq->putQuota('ec2', 'L-0263D0A3', new PutServiceQuotaRequest(enforcement: QuotaEnforcement::Ignore));
        $this->assertSame(['enforce' => false], $this->lastBody());
    }

    public function testPutQuotaEncodesPathSegments(): void
    {
        $this->respond(200, self::quota());
        $this->sq->putQuota('a b', 'c/d', new PutServiceQuotaRequest(value: 1.0));
        $this->assertSame('/_fakecloud/service-quotas/quotas/a%20b/c%2Fd', $this->lastRequest()['uri']);
    }

    public function testDeleteQuota(): void
    {
        $this->respond(200, self::quota(['appliedValue' => 5.0, 'enforcementSource' => 'global']));
        $q = $this->sq->deleteQuota('ec2', 'L-0263D0A3', accountId: '111122223333', region: 'eu-west-1');
        $req = $this->lastRequest();
        $this->assertSame('DELETE', $req['method']);
        $this->assertSame(
            '/_fakecloud/service-quotas/quotas/ec2/L-0263D0A3?accountId=111122223333&region=eu-west-1',
            $req['uri'],
        );
        $this->assertSame(5.0, $q->appliedValue);
        $this->assertSame('global', $q->enforcementSource);
    }

    public function testGetEnforcement(): void
    {
        $this->respond(200, [
            'enforceAll' => true,
            'overrides' => [['serviceCode' => 'ec2', 'quotaCode' => 'L-1', 'enforce' => false]],
            'accountOverrides' => [['accountId' => '111122223333', 'serviceCode' => 'ec2', 'quotaCode' => 'L-2', 'enforce' => true]],
        ]);
        $resp = $this->sq->getEnforcement();
        $this->assertSame('GET', $this->lastRequest()['method']);
        $this->assertSame('/_fakecloud/service-quotas/enforcement', $this->lastRequest()['uri']);
        $this->assertTrue($resp->enforceAll);
        $this->assertSame('L-1', $resp->overrides[0]->quotaCode);
        $this->assertFalse($resp->overrides[0]->enforce);
        $this->assertSame('111122223333', $resp->accountOverrides[0]->accountId);
        $this->assertTrue($resp->accountOverrides[0]->enforce);
    }

    public function testPutEnforcement(): void
    {
        $this->respond(200, ['enforceAll' => false, 'overrides' => [], 'accountOverrides' => []]);
        $resp = $this->sq->putEnforcement(new PutServiceQuotaEnforcementRequest(
            enforceAll: false,
            overrides: [
                new ServiceQuotaEnforcementChange('ec2', 'L-1', QuotaEnforcement::Enforce),
                new ServiceQuotaEnforcementChange('ec2', 'L-2', QuotaEnforcement::Default, accountId: '111122223333'),
            ],
        ));
        $req = $this->lastRequest();
        $this->assertSame('PUT', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/enforcement', $req['uri']);
        $this->assertSame([
            'enforceAll' => false,
            'overrides' => [
                ['serviceCode' => 'ec2', 'quotaCode' => 'L-1', 'enforce' => true],
                ['serviceCode' => 'ec2', 'quotaCode' => 'L-2', 'accountId' => '111122223333', 'enforce' => null],
            ],
        ], $this->lastBody());
        $this->assertFalse($resp->enforceAll);
    }

    public function testPutEnforcementEmptySendsObject(): void
    {
        $this->respond(200, ['enforceAll' => false, 'overrides' => [], 'accountOverrides' => []]);
        $this->sq->putEnforcement(new PutServiceQuotaEnforcementRequest());
        $this->assertSame('{}', $this->lastRequest()['body']);
    }

    public function testRequestApproval(): void
    {
        $this->respond(200, ['mode' => 'manual']);
        $resp = $this->sq->setRequestApproval('manual');
        $req = $this->lastRequest();
        $this->assertSame('PUT', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/request-approval', $req['uri']);
        $this->assertSame(['mode' => 'manual'], $this->lastBody());
        $this->assertSame('manual', $resp->mode);

        $this->respond(200, ['mode' => 'auto']);
        $this->assertSame('auto', $this->sq->getRequestApproval()->mode);
        $this->assertSame('GET', $this->lastRequest()['method']);
    }

    public function testGetRequests(): void
    {
        $this->respond(200, ['requests' => [self::request(['status' => 'PENDING', 'caseId' => 'case-1', 'region' => ''])]]);
        $resp = $this->sq->getRequests(accountId: '123456789012', status: 'PENDING');
        $this->assertSame(
            '/_fakecloud/service-quotas/requests?accountId=123456789012&status=PENDING',
            $this->lastRequest()['uri'],
        );
        $r = $resp->requests[0];
        $this->assertSame('req-1', $r->requestId);
        $this->assertSame('PENDING', $r->status);
        $this->assertSame('case-1', $r->caseId);
        $this->assertSame('', $r->region);
        $this->assertSame(10.0, $r->desiredValue);
        $this->assertSame('2026-10-05T12:01:00+00:00', $r->lastUpdated);
    }

    public function testApproveRequest(): void
    {
        $this->respond(200, self::request());
        $r = $this->sq->approveRequest('req/1');
        $req = $this->lastRequest();
        $this->assertSame('POST', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/requests/req%2F1/approve', $req['uri']);
        $this->assertSame('', $req['body']);
        $this->assertSame('APPROVED', $r->status);
        $this->assertNull($r->caseId);
    }

    public function testDenyRequestDefaultStatus(): void
    {
        $this->respond(200, self::request(['status' => 'DENIED']));
        $r = $this->sq->denyRequest('req-1');
        $req = $this->lastRequest();
        $this->assertSame('POST', $req['method']);
        $this->assertSame('/_fakecloud/service-quotas/requests/req-1/deny', $req['uri']);
        $this->assertSame('', $req['body']);
        $this->assertSame('DENIED', $r->status);
    }

    public function testDenyRequestWithStatus(): void
    {
        $this->respond(200, self::request(['status' => 'CASE_CLOSED']));
        $r = $this->sq->denyRequest('req-1', 'CASE_CLOSED');
        $this->assertSame(['status' => 'CASE_CLOSED'], $this->lastBody());
        $this->assertSame('CASE_CLOSED', $r->status);
    }

    public function testErrorPropagation(): void
    {
        $this->respond(400, ['error' => 'nothing to change: give value and/or enforce']);
        try {
            $this->sq->putQuota('ec2', 'L-0263D0A3', new PutServiceQuotaRequest());
            $this->fail('expected FakeCloudError');
        } catch (FakeCloudError $e) {
            $this->assertSame(400, $e->status);
            $this->assertSame('{"error":"nothing to change: give value and\/or enforce"}', $e->body);
        }
    }

    public function testConflictPropagation(): void
    {
        $this->respond(409, ['error' => 'quota request req-1 is already decided (APPROVED)']);
        $this->expectException(FakeCloudError::class);
        $this->expectExceptionMessage('fakecloud API error (409)');
        $this->sq->approveRequest('req-1');
    }
}
